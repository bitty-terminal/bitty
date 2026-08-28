//! Owned runtime orchestration: PTY, parser, terminal state, renderer, and surface.
//!
//! This module owns the Correct Terminal data flow described in the
//! terminal-state-rfc pipeline overview:
//!
//! ```text
//! PTY bytes -> VT Parser -> TerminalAction -> Terminal State -> Snapshot + Damage
//!                                  |                |
//!                                  v                v
//!                         cold-path queue      GridRenderer -> DrawList -> Surface::present
//!                                  |                ^
//!                                  +--> bounded side queue --> PluginHost (draft)
//!                                         |   owned EventPipeline + SideQueue<HostObservation>
//!                                  v
//!                                   grant checks / DropPolicy DropOldest (accepted v1 default, OQ-013 closed) / interception stubs
//! ```
//!
//! The hot path never touches Lua, plugins, or the cold queue beyond pushing
//! bounded events. The cold-path queue is strictly bounded so untrusted PTY
//! bytes cannot grow the heap without limit (T-01). When full, the oldest
//! event is dropped and a counter increments, mirroring terminal-state's
//! reply-cap policy.
//!
//! Multi-pane extension (CTX-0023): `Runtime` owns a `LayoutNode` tree and a
//! `Focus` state. `set_layout` replaces the tree; `tick` reflows the tree
//! into the current container `Rect` (cell coordinates) via `LayoutNode::reflow`,
//! then renders per leaf: each leaf `View`'s `cols`/`rows` and `origin` are
//! updated to its allocation, a viewport snapshot slice is rendered through the
//! shared `GridRenderer` (translated to the leaf's pixel origin), and the
//! combined `DrawList` is presented once via the headless software seam.
//! Layout math remains headless-testable without GPU/window; the software
//! present proves split/stack/overlay composition.
//!
//! # Plugin-host wiring (CTX-0027) — draft status, experimental review evidence
//!
//! This module owns a [`bitty_plugin_host::PluginHost`] behind the cold path.
//! The host tracks the `plugin-platform-rfc.md` contract
//! (`Proposed` / `draft`, `OQ-011..OQ-013`, `OQ-014`). The wiring is headless-testable
//! and introduces no window, GPU, or Lua VM coupling:
//!
//! - **Owned host:** `Runtime` owns one `PluginHost` (always present, not feature-gated
//!   for this draft slice). Construction uses the **accepted v1 default**
//!   [`bitty_plugin_host::DropPolicy::DropOldest`] with per-queue `64` and side queue `128`
//!   (experimental implementation as review evidence per the new RFC lifecycle
//!   `Draft -> experimental review evidence -> Accepted -> normative`;
//!   `plugin-platform-rfc.md` remains `Proposed` until independent review).
//!   This choice **closes `OQ-013` § “Delivery, ordering, batching, and coalescing”**
//!   (point 3) as the accepted v1 default. Callers that
//!   need `DropNewest` must construct via [`Runtime::with_plugin_host`] / [`Runtime::with_plugin_drop_policy`]
//!   or replace the host via [`Runtime::plugin_host_mut`]; `DropNewest` remains
//!   available via explicit opt-in but is not the v1 default.
//! - **Cold → side bridging (ADR-0003 rule 4):** `handle_pty_bytes` pushes bounded
//!   [`crate::queue::ColdEvent`]s to the `ColdQueue` *and* non-blocking bounded
//!   [`bitty_plugin_host::HostObservation`]s to the host's [`bitty_plugin_host::SideQueue`].
//!   The side queue is strictly bounded and never blocks the producer; when full the
//!   oldest observation is dropped and the count is exposed for `bitty plugin doctor` via
//!   [`Runtime::plugin_side_dropped`]/[`Runtime::plugin_total_dropped`].
//! - **Event routing:** `register_plugin` validates and registers a manifest via
//!   `declare → resolve → register`; subscriptions and publishing go through the
//!   host's [`bitty_plugin_host::EventPipeline`]. Interception handlers are synchronous,
//!   veto-wins, fail-open, and remain cold-path only (the four v1 points
//!   `intercept.command-dispatch/terminal-spawn/paste/open-url`).
//! - **Grant stubs:** `is_capability_granted`, `insert_grant`, `revoke_grant`, and
//!   `dispatch_command` (grant-checked) are headless stubs with no file I/O; they
//!   intersect the manifest's declared capabilities with the grant store.
//! - **No hot-path coupling:** plugins never observe `byte-received` / `cell-changed` /
//!   per-byte signals; only bounded post-state observations cross the queue.
//! - **Honest gaps:** Lua VM creation/execution, real capability consent UX, handler
//!   execution with budgets/timeouts (OQ-014), and actual command invocation via the
//!   plugin VM remain deferred. This slice only wires the host-owned data structures
//!   and the bounded crossing.

use bitty_platform::{KeyEvent, PhysicalSize, PlatformEvent, WindowEventKind};
use bitty_pty::{Pty, PtyBuilder, PtyReader, PtyWriter};
use bitty_render::{
    RenderError,
    frame::{FrameMode, FramePlan},
    glyph::{
        BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
        RasterKey,
    },
    gpu::{PresentStats as RenderPresentStats, Surface},
    grid::{CellMetrics, DrawList, GridRenderer},
};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction};
use bitty_ui::{Focus, FocusDirection, LayoutNode, Rect as UiRect, View, ViewId};
use bitty_vt::{Parser, SequenceKind};

use bitty_plugin_host::{
    CapabilityId, DropPolicy, Event, EventKind, GrantRecord, HostObservation, InterceptionDecision,
    PluginHost, PluginId, PluginManifest, QualifiedName,
};

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::queue::{ColdEvent, ColdQueue};

/// Deterministic headless rasterizer: no font stack required, bit-identical
/// on every platform and on both Linux and Windows CI.
///
/// Blank characters (`' '` and zero-width-adjacent) return `None` (cacheable
/// miss). All other characters produce a deterministic square coverage mask
/// derived from the scalar value, sized `6..8` pixels `+` font-size scaling.
/// The bitmap is `Rgb` coverage averaged to luminance by the software
/// compositor, exactly as the GPU path will sample atlas texels.
#[derive(Debug)]
struct HeadlessRasterizer {
    next_id: u64,
}

impl HeadlessRasterizer {
    fn new() -> Self {
        Self { next_id: 0 }
    }
}

impl GlyphRasterizer for HeadlessRasterizer {
    fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next_id))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' || key.character == '\0' {
            return Ok(None);
        }
        // Deterministic size: 6..8 + small font-size contribution so different
        // point sizes hash to different bitmaps without affecting determinism
        // across platforms (no font metrics involved).
        let base = (u32::from(key.character) % 3 + 6) as i32;
        // Slight size bump from point_size to keep per-size raster keys distinct
        // without breaking the bounded bit model.
        let varied = if key.point_size > 13.0 {
            base + 1
        } else {
            base
        };
        let side = varied;
        let channels = BitmapFormat::Rgb.channels();
        let data_len = side as usize * side as usize * channels;
        let data = vec![0xAA; data_len];
        let metrics = GlyphMetrics {
            left: 0,
            top: 6,
            width: side,
            height: side,
            advance: [side, 0],
        };
        Ok(Some(
            GlyphBitmap::try_new(metrics, BitmapFormat::Rgb, data)
                .expect("deterministic bitmap must be valid"),
        ))
    }
}

/// Owned presentation statistics returned by [`Runtime::tick`].
///
/// This type is workspace-owned; no `wgpu` type leaks through it. The
/// `headless` flag is `true` for the software seam that CI exercises; a
/// real GPU present sets it to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentStats {
    /// Logical frame counter for the surface.
    pub frame: u64,
    /// Number of fill rectangles in the presented draw list.
    pub fills: usize,
    /// Number of glyph instances in the presented draw list.
    pub glyphs: usize,
    /// Whether the surface was the headless software fake.
    pub headless: bool,
    /// Snapshot generation that was presented.
    pub generation: u64,
}

impl From<RenderPresentStats> for PresentStats {
    fn from(value: RenderPresentStats) -> Self {
        Self {
            frame: value.frame,
            fills: value.fills,
            glyphs: value.glyphs,
            headless: value.headless,
            generation: 0,
        }
    }
}

fn default_layout(cols: usize, rows: usize) -> LayoutNode {
    let view = View::new(ViewId::new(1), cols, rows);
    LayoutNode::leaf(view)
}

fn default_container(cols: usize, rows: usize) -> UiRect {
    let w = cols.min(u16::MAX as usize) as u16;
    let h = rows.min(u16::MAX as usize) as u16;
    UiRect::new(0, 0, w, h)
}

/// Creates a viewport snapshot of `snapshot` limited to `cols x rows`.
///
/// The viewport is the top-left `cols x rows` window of the active screen,
/// padded with erased cells when the requested size exceeds the snapshot
/// dimensions (honest padding for the deferred grid-resize reflow). Cursor and
/// modes are carried over; title/modes are snapshot-owned.
fn viewport_snapshot(snapshot: &Snapshot, cols: u16, rows: u16) -> Snapshot {
    let req_w = cols as usize;
    let req_h = rows as usize;
    if snapshot.width == req_w && snapshot.height == req_h {
        return snapshot.clone();
    }
    let mut cells = Vec::with_capacity(req_w * req_h);
    let src_w = snapshot.width;
    let src_h = snapshot.height;
    for r in 0..req_h {
        for c in 0..req_w {
            if r < src_h && c < src_w {
                let idx = r * src_w + c;
                let cell = snapshot.cells.get(idx).cloned().unwrap_or_else(|| {
                    bitty_term_state::Cell::erased(bitty_term_state::Style::default())
                });
                // For the trailing spacer of a wide char that would be split
                // across the viewport edge, degrade to an erased cell to keep
                // invariants (no orphan spacer): the leading half outside the
                // viewport is not visible, so the spacer inside is erased.
                if cell.spacer && c == 0 {
                    cells.push(bitty_term_state::Cell::erased(cell.style));
                } else {
                    // If this is a wide leading cell at the right edge and its
                    // spacer would fall outside the viewport, truncate to single
                    // width to avoid unpaired wide.
                    let mut out = cell;
                    if out.width == 2 && c + 1 >= req_w && !out.spacer {
                        out.width = 1;
                    }
                    cells.push(out);
                }
            } else {
                cells.push(bitty_term_state::Cell::erased(
                    bitty_term_state::Style::default(),
                ));
            }
        }
    }
    Snapshot {
        version: snapshot.version,
        generation: snapshot.generation,
        width: req_w,
        height: req_h,
        cells: cells.into_boxed_slice(),
        cursor: snapshot.cursor.clone(),
        modes: snapshot.modes.clone(),
        title: snapshot.title.clone(),
    }
}

/// The Correct Terminal orchestration: owns PTY, parser, terminal state,
/// renderer, surface, and the bounded cold-path queue.
///
/// # Ownership
///
/// - **PTY**: optional until [`Runtime::spawn_shell`] succeeds; process
///   lifecycle, resize, and backpressure are encapsulated in `bitty-pty`.
/// - **Parser + State**: the only write path into terminal truth; `State`
///   is mutated exclusively through `Parser`-produced `TerminalAction`s.
/// - **Renderer + Surface**: the renderer consumes `Snapshot + Damage`
///   only; the surface is always the headless software fake in this slice.
///   A real GPU surface requires an async `GpuContext::initialize` and a
///   live `SurfaceTarget` — both are honest env-gated gaps documented
///   below and remain unavailable on headless CI.
/// - **Cold queue**: bounded, drop-oldest when full, observed by the future
///   plugin host without ever borrowing hot-path state mutably.
/// - **Layout + Focus**: owned `LayoutNode` tree and `Focus` state. The tree
///   is deterministically laid out into the current container `Rect` via
///   `LayoutNode::reflow`; per-leaf tick renders each `View` allocation.
///
/// # Threading
///
/// This type is `!Send` only because `bitty_render::grid::GridRenderer`
/// contains a cache whose rasterizer is not `Send` today. Headless tests
/// drive the runtime on one thread. Future slices that need cross-thread
/// ownership will parameterise the rasterizer or wrap access.
///
/// # Headless vs real split (honest)
///
/// - **Headless (CI, default):** `Runtime::new` builds a `Surface::headless`
///   with the config-derived pixel extent and a deterministic rasterizer.
///   `tick` composites `DrawList + Atlas` onto an in-memory RGBA buffer via
///   `Surface::headless_present`. No display server, window, adapter, or
///   font file is contacted. This proves the full byte-to-photon path
///   without GPU. It is the only path CI verifies.
/// - **Real (env-gated):** attaching a real window surface requires
///   `GpuContext::initialize().await` on a machine with a working driver and
///   a live `SurfaceTarget` from `bitty_platform::WindowHandle`. Those APIs
///   return `RenderError::NoCompatibleAdapter` on headless runners and are
///   covered only by manual or env-gated tests (`BITTY_RENDER_GPU_TESTS=1`
///   in `bitty-render`). This crate does not yet expose an `attach_gpu`
///   API; callers must not describe it as implemented.
///
/// Accepted v1 defaults for the plugin-host wiring (experimental review evidence).
/// These satisfy bounded-queue invariants and are headless-testable; pipeline
/// `64` / side `128` and batch `32`/`8 KiB` remain the OQ-014 candidate values
/// used as the v1 baseline, while the drop policy is OQ-013 closed.
pub const DEFAULT_PLUGIN_PIPELINE_CAPACITY: usize = bitty_plugin_host::DEFAULT_QUEUE_CAPACITY;
/// Side queue capacity for [`HostObservation`] (ADR-0003 rule 4).
pub const DEFAULT_PLUGIN_SIDE_CAPACITY: usize = 128;
/// Accepted v1 default drop policy — `DropOldest` (OQ-013 closed decision point).
///
/// Experimental implementation as review evidence per the new RFC lifecycle
/// (`Draft -> experimental review evidence -> Accepted -> normative`);
/// `plugin-platform-rfc.md` remains `Proposed`/`draft` until independent
/// review (category owner + docs curator + security reviewer).
pub const DEFAULT_PLUGIN_DROP_POLICY: DropPolicy = DropPolicy::DropOldest;

/// Map a [`ColdEvent`] to a [`HostObservation`] where semantics overlap.
///
/// Only post-state, bounded observations cross the queue; hot-path payloads
/// (bytes, cells, per-frame damage beyond generation) never cross. Returns `None`
/// when there is no direct observation mapping (e.g. `ZoneMarked`, `HyperlinkChanged`).
fn cold_to_observation(event: &ColdEvent) -> Option<HostObservation> {
    match event {
        ColdEvent::TitleChanged(s) => Some(HostObservation::TitleChanged(s.clone())),
        ColdEvent::CwdChanged(s) => Some(HostObservation::CwdChanged(s.clone())),
        ColdEvent::Bell => Some(HostObservation::Bell),
        ColdEvent::ModeChanged { mode, enabled } => Some(HostObservation::ModeChanged {
            mode: format!("{mode:?}"),
            enabled: *enabled,
        }),
        ColdEvent::Damage { generation } => Some(HostObservation::Damage {
            generation: *generation,
        }),
        ColdEvent::ZoneMarked(_)
        | ColdEvent::HyperlinkChanged(_)
        | ColdEvent::UnknownSequence(_) => None,
    }
}

/// Bounded pending input buffer (keyboard bytes awaiting PTY write or
/// headless observation). Mirrors the cold-queue bound philosophy (T-01) but
/// for the input path; 8 KiB is enough for burst typing without unbounded
/// growth. When full, oldest bytes are dropped and counted via
/// `pending_input_dropped`.
const MAX_PENDING_INPUT: usize = 8192;

pub struct Runtime {
    config: RuntimeConfig,
    parser: Parser,
    state: State,
    pty: Option<Pty>,
    pty_reader: Option<PtyReader>,
    pty_writer: Option<PtyWriter>,
    pending_input: Vec<u8>,
    pending_input_dropped: u64,
    renderer: GridRenderer<HeadlessRasterizer>,
    surface: Surface,
    cold_queue: ColdQueue,
    plugin_host: PluginHost,
    last_presented_generation: u64,
    pending_full_redraw: bool,
    cols: usize,
    rows: usize,
    layout: LayoutNode,
    focus: Focus,
    container: UiRect,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("generation", &self.state.generation())
            .field("cold_queue_len", &self.cold_queue.len())
            .field("plugin_side_len", &self.plugin_host.side_queue().len())
            .field(
                "plugin_side_dropped",
                &self.plugin_host.side_queue().dropped(),
            )
            .field(
                "plugin_pipeline_dropped",
                &self.plugin_host.pipeline().total_dropped(),
            )
            .field("has_pty", &self.pty.is_some())
            .field("has_pty_reader", &self.pty_reader.is_some())
            .field("has_pty_writer", &self.pty_writer.is_some())
            .field("pending_input_len", &self.pending_input.len())
            .field("pending_input_dropped", &self.pending_input_dropped)
            .field("pending_full_redraw", &self.pending_full_redraw)
            .field("leaf_count", &self.layout.leaf_count())
            .field("focused", &self.focus.focused())
            .field("container", &self.container)
            .field(
                "plugin_drop_policy",
                &self.plugin_host.pipeline().drop_policy(),
            )
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Creates a runtime from `config`, validating the config eagerly and
    /// building the headless software surface and deterministic renderer.
    ///
    /// The initial layout is a single leaf `ViewId(1)` sized to `config`
    /// cols/rows with focus on that leaf and a container matching the grid.
    /// The owned [`PluginHost`] is created with the **accepted v1 default**
    /// [`DEFAULT_PLUGIN_DROP_POLICY`] (`DropOldest`, OQ-013 closed decision
    /// point; experimental implementation as review evidence per the new RFC
    /// lifecycle `Draft -> experimental review evidence -> Accepted -> normative`;
    /// `plugin-platform-rfc.md` remains `Proposed` until independent review),
    /// pipeline capacity [`DEFAULT_PLUGIN_PIPELINE_CAPACITY`] (64) and side
    /// capacity [`DEFAULT_PLUGIN_SIDE_CAPACITY`] (128). `DropNewest` remains
    /// available via explicit opt-in through
    /// [`Self::with_plugin_drop_policy`] or [`Self::with_plugin_host`].
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] for bad grid or font fields;
    /// [`RuntimeError::Render`] when the surface or renderer construction
    /// rejects the derived pixel extent or font query.
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        Self::with_plugin_drop_policy(config, DEFAULT_PLUGIN_DROP_POLICY)
    }

    /// Creates a runtime with an explicit [`DropPolicy`] for the plugin host.
    ///
    /// The caller chooses the queue-overflow policy explicitly. `DropOldest`
    /// is the accepted v1 default (OQ-013 closed decision point; experimental
    /// implementation as review evidence per the new RFC lifecycle
    /// `Draft -> experimental review evidence -> Accepted -> normative` and
    /// RFC § “Delivery, ordering, batching, and coalescing” point 3;
    /// `plugin-platform-rfc.md` remains `Proposed` until independent review).
    /// `DropNewest` is available via explicit opt-in; this constructor makes
    /// the choice visible at the call site.
    pub fn with_plugin_drop_policy(
        config: RuntimeConfig,
        drop_policy: DropPolicy,
    ) -> Result<Self, RuntimeError> {
        Self::with_plugin_host_capacity(
            config,
            drop_policy,
            DEFAULT_PLUGIN_PIPELINE_CAPACITY,
            DEFAULT_PLUGIN_SIDE_CAPACITY,
        )
    }

    /// Creates a runtime with explicit plugin-host capacities and drop policy.
    ///
    /// `pipeline_capacity` bounds each per-subscriber event queue; `side_capacity`
    /// bounds the [`HostObservation`] side queue per ADR-0003 rule 4
    /// (hot path never blocks, drops counted for `bitty plugin doctor`).
    pub fn with_plugin_host_capacity(
        config: RuntimeConfig,
        drop_policy: DropPolicy,
        pipeline_capacity: usize,
        side_capacity: usize,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        if pipeline_capacity == 0 || side_capacity == 0 {
            return Err(RuntimeError::InvalidQueueCapacity);
        }
        let extent = config.pixel_extent();
        let surface = Surface::headless(extent).map_err(RuntimeError::from)?;
        let cell = CellMetrics::new(config.cell_width, config.cell_height)
            .expect("validated config guarantees non-zero cell metrics");
        let query = FontQuery {
            family: config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: config.font_size,
        };
        let renderer = GridRenderer::new(HeadlessRasterizer::new(), &query, cell)
            .map_err(RuntimeError::from)?;
        let cols = config.cols;
        let rows = config.rows;
        let layout = default_layout(cols, rows);
        let focus = Focus::with_focus(ViewId::new(1));
        let container = default_container(cols, rows);
        let plugin_host = PluginHost::with_capacity(drop_policy, pipeline_capacity, side_capacity);
        Ok(Self {
            cols,
            rows,
            config: config.clone(),
            parser: Parser::new(),
            state: State::new(),
            pty: None,
            pty_reader: None,
            pty_writer: None,
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            pending_full_redraw: true,
            layout,
            focus,
            container,
        })
    }

    /// Creates a runtime that takes ownership of an already-constructed [`PluginHost`].
    ///
    /// Headless tests may pre-populate the host (grants, safe mode) before handing it to
    /// the runtime; this constructor preserves that state and does not re-create capacities.
    pub fn with_plugin_host(
        config: RuntimeConfig,
        plugin_host: PluginHost,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        let extent = config.pixel_extent();
        let surface = Surface::headless(extent).map_err(RuntimeError::from)?;
        let cell = CellMetrics::new(config.cell_width, config.cell_height)
            .expect("validated config guarantees non-zero cell metrics");
        let query = FontQuery {
            family: config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: config.font_size,
        };
        let renderer = GridRenderer::new(HeadlessRasterizer::new(), &query, cell)
            .map_err(RuntimeError::from)?;
        let cols = config.cols;
        let rows = config.rows;
        let layout = default_layout(cols, rows);
        let focus = Focus::with_focus(ViewId::new(1));
        let container = default_container(cols, rows);
        Ok(Self {
            cols,
            rows,
            config: config.clone(),
            parser: Parser::new(),
            state: State::new(),
            pty: None,
            pty_reader: None,
            pty_writer: None,
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            pending_full_redraw: true,
            layout,
            focus,
            container,
        })
    }

    /// Convenience: default config runtime.
    ///
    /// # Errors
    ///
    /// Same as [`Runtime::new`]; default config is expected to succeed on
    /// every platform.
    pub fn with_defaults() -> Result<Self, RuntimeError> {
        Self::new(RuntimeConfig::default())
    }

    /// Owned config view.
    #[must_use]
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Snapshot of terminal truth for renderers or tests.
    #[must_use]
    pub fn snapshot(&self) -> bitty_term_state::Snapshot {
        self.state.snapshot()
    }

    /// Current terminal state (read-only) for assertions.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Current surface extent, if the surface has been configured.
    #[must_use]
    pub fn surface_extent(&self) -> Option<PhysicalSize> {
        self.surface.extent()
    }

    /// Whether the surface is the headless software fake.
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.surface.is_headless()
    }

    /// Number of queued cold-path events.
    #[must_use]
    pub fn cold_queue_len(&self) -> usize {
        self.cold_queue.len()
    }

    /// Capacity of the cold-path queue.
    #[must_use]
    pub fn cold_queue_capacity(&self) -> usize {
        self.cold_queue.capacity()
    }

    /// How many cold events have been dropped due to overflow.
    #[must_use]
    pub fn cold_queue_dropped(&self) -> u64 {
        self.cold_queue.dropped()
    }

    /// Drains all queued cold-path events in FIFO order.
    pub fn drain_cold_events(&mut self) -> Vec<ColdEvent> {
        self.cold_queue.drain()
    }

    /// Returns the in-memory RGBA buffer of the last presented headless frame,
    /// if any (premultiplied, `width*height*4` bytes, row-major RGBA).
    #[must_use]
    pub fn headless_rgba(&self) -> Option<Vec<u8>> {
        let raw = self.surface.headless_rgba()?;
        Some(raw)
    }

    // ------------------------------------------------------------------
    // Keyboard input (CTX-0057) — winit → owned KeyEvent → legacy VT bytes → PTY
    // ------------------------------------------------------------------

    /// Encodes a [`KeyEvent`] into the terminal input bytes (legacy xterm).
    ///
    /// Pure, headless, and deterministic: delegates to
    /// [`bitty_platform::encode_key_event`] which owns the xterm legacy table
    /// (M1 required baseline; Kitty protocol is deferred). Returns `None` for
    /// release/synthetic/modifier-only/unmapped inputs.
    #[must_use]
    pub fn encode_key_event(event: &KeyEvent) -> Option<Vec<u8>> {
        bitty_platform::encode_key_event(event)
    }

    /// Handles a decoded keyboard event: encodes to bytes and routes to the PTY.
    ///
    /// When a live PTY writer exists the bytes are written directly (best
    /// effort; errors are ignored so a transient write failure never panics
    /// the loop). Otherwise the bytes are buffered in a bounded
    /// `pending_input` queue (`MAX_PENDING_INPUT` = 8 KiB) so headless
    /// synthetic tests can observe them via [`Self::drain_pending_input`].
    /// When the buffer would overflow the oldest bytes are dropped and
    /// [`Self::pending_input_dropped`] increments (drop-oldest).
    ///
    /// Returns the encoded bytes when the event produced input, `None`
    /// otherwise (release, synthetic, modifier-only, etc.). Headless
    /// callers may synthesize [`KeyEvent`]s without a window and drive this
    /// path deterministically.
    pub fn handle_key_event(&mut self, event: KeyEvent) -> Option<Vec<u8>> {
        let bytes = Self::encode_key_event(&event)?;
        self.push_input_bytes(&bytes);
        Some(bytes)
    }

    /// Convenience: handles a borrowed [`KeyEvent`] without moving it.
    pub fn handle_key_event_ref(&mut self, event: &KeyEvent) -> Option<Vec<u8>> {
        let bytes = Self::encode_key_event(event)?;
        self.push_input_bytes(&bytes);
        Some(bytes)
    }

    /// Pushes raw input bytes into the pending queue and, when a PTY writer
    /// is live, writes them through.
    pub fn push_input_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Try live PTY write first (best effort, never panics).
        if let Some(writer) = self.pty_writer.as_mut() {
            use std::io::Write as _;
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
            // Also keep a copy in pending for observability? For live PTY
            // tests the child will echo, so pending is not needed. We do not
            // double-buffer when writer exists to keep the bound honest.
            // Headless tests without a writer will observe via pending.
            return;
        }
        // Headless / no-writer path: bounded buffer with drop-oldest.
        let overflow = self.pending_input.len() + bytes.len() > MAX_PENDING_INPUT;
        if overflow {
            // Make room by dropping oldest bytes.
            let needed = self.pending_input.len() + bytes.len() - MAX_PENDING_INPUT;
            let drop = needed.min(self.pending_input.len());
            self.pending_input.drain(0..drop);
            self.pending_input_dropped += drop as u64;
            // If the incoming chunk itself exceeds capacity, truncate to last
            // MAX_PENDING_INPUT bytes (still bounded).
            if bytes.len() > MAX_PENDING_INPUT {
                let start = bytes.len() - MAX_PENDING_INPUT;
                let dropped_extra = bytes.len() - MAX_PENDING_INPUT;
                self.pending_input_dropped += dropped_extra as u64;
                self.pending_input.extend_from_slice(&bytes[start..]);
                return;
            }
        }
        self.pending_input.extend_from_slice(bytes);
    }

    /// Number of bytes currently buffered for PTY input (headless observation).
    #[must_use]
    pub fn pending_input_len(&self) -> usize {
        self.pending_input.len()
    }

    /// How many input bytes have been dropped due to buffer overflow.
    #[must_use]
    pub fn pending_input_dropped(&self) -> u64 {
        self.pending_input_dropped
    }

    /// Views the pending input buffer without draining (headless helper).
    #[must_use]
    pub fn pending_input(&self) -> &[u8] {
        &self.pending_input
    }

    /// Drains and returns all pending input bytes (headless helper).
    pub fn drain_pending_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_input)
    }

    /// Whether a live PTY writer is currently owned.
    #[must_use]
    pub fn has_pty_writer(&self) -> bool {
        self.pty_writer.is_some()
    }

    /// Takes exclusive ownership of the PTY writer, if present (test helper).
    pub fn take_pty_writer(&mut self) -> Option<PtyWriter> {
        self.pty_writer.take()
    }

    /// Writes raw bytes as terminal input, same routing as keyboard (writer
    /// when live, bounded pending otherwise).
    pub fn write_input(&mut self, bytes: &[u8]) {
        self.push_input_bytes(bytes);
    }

    // ------------------------------------------------------------------
    // Plugin-host wiring (CTX-0027) — draft, headless, no window/GPU/Lua
    // ------------------------------------------------------------------

    /// Owned plugin host (read-only).
    #[must_use]
    pub fn plugin_host(&self) -> &PluginHost {
        &self.plugin_host
    }

    /// Owned plugin host (mutable).
    #[must_use]
    pub fn plugin_host_mut(&mut self) -> &mut PluginHost {
        &mut self.plugin_host
    }

    /// Drop policy for the plugin host's event pipeline (accepted v1 default `DropOldest`, OQ-013 closed).
    #[must_use]
    pub fn plugin_drop_policy(&self) -> DropPolicy {
        self.plugin_host.pipeline().drop_policy()
    }

    /// Per-queue capacity for the plugin pipeline (candidate, `OQ-014`; budget not yet normative).
    #[must_use]
    pub fn plugin_pipeline_capacity(&self) -> usize {
        self.plugin_host.pipeline().default_capacity()
    }

    /// Side-queue capacity for [`HostObservation`] (ADR-0003 rule 4).
    #[must_use]
    pub fn plugin_side_capacity(&self) -> usize {
        self.plugin_host.side_queue().capacity()
    }

    /// Number of queued [`HostObservation`]s in the side queue.
    #[must_use]
    pub fn plugin_side_len(&self) -> usize {
        self.plugin_host.side_queue().len()
    }

    /// How many side-queue observations have been dropped (bounded, counted for `bitty plugin doctor`).
    #[must_use]
    pub fn plugin_side_dropped(&self) -> u64 {
        self.plugin_host.side_queue().dropped()
    }

    /// Total dropped events across all per-subscriber pipeline queues (for `bitty plugin doctor`).
    #[must_use]
    pub fn plugin_total_dropped(&self) -> u64 {
        self.plugin_host.pipeline().total_dropped()
    }

    /// Per-queue dropped counts `(plugin_id, event_kind) -> dropped`.
    #[must_use]
    pub fn plugin_dropped_per_queue(&self) -> std::collections::BTreeMap<(String, String), u64> {
        self.plugin_host.pipeline().dropped_per_queue()
    }

    /// Whether the host is in safe mode (`bitty --safe` skips third-party plugins).
    #[must_use]
    pub fn plugin_safe_mode(&self) -> bool {
        self.plugin_host.is_safe_mode()
    }

    /// Enable or disable safe mode.
    pub fn set_plugin_safe_mode(&mut self, safe: bool) {
        self.plugin_host.set_safe_mode(safe);
    }

    /// Register a plugin from its already-parsed manifest.
    ///
    /// Validates the manifest, inserts as `Declared`, resolves dependencies
    /// against the current registry, and reserves commands/event subscriptions
    /// at graph construction time (duplicate qualified names are rejected here,
    /// not shadowed). On success the entry is `Registered`; caller may then
    /// [`Self::activate_plugin`] or let the lazy loader activate on first
    /// command/event.
    ///
    /// Headless-testable: no file I/O, no VM, no window/GPU.
    pub fn register_plugin(&mut self, manifest: PluginManifest) -> Result<(), RuntimeError> {
        let id = manifest.id().clone();
        self.plugin_host.declare(manifest)?;
        // Resolve may fail if dependencies missing; we keep the `Declared` entry
        // and surface the error for the caller to inspect `plugin_host.registry()`.
        self.plugin_host.resolve(&id)?;
        self.plugin_host.register(&id)?;
        Ok(())
    }

    /// Activate a previously registered plugin (moves `Registered -> Activated`).
    pub fn activate_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.activate(id).map_err(RuntimeError::from)
    }

    /// Suspend a plugin.
    pub fn suspend_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.suspend(id).map_err(RuntimeError::from)
    }

    /// Resume a suspended plugin (`Suspended -> Registered`, caller may `activate` again).
    pub fn resume_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.resume(id).map_err(RuntimeError::from)
    }

    /// Dispose a plugin (releases generation resources).
    pub fn dispose_plugin(&mut self, id: &PluginId) -> Result<(), RuntimeError> {
        self.plugin_host.dispose(id).map_err(RuntimeError::from)
    }

    /// Subscribe `plugin_id` to `kind` (requires the event was declared in the manifest).
    pub fn subscribe_plugin_event(
        &mut self,
        plugin_id: &PluginId,
        kind: EventKind,
    ) -> Result<(), RuntimeError> {
        self.plugin_host
            .subscribe(plugin_id, kind)
            .map_err(RuntimeError::from)
    }

    /// Publish an event to all subscribers of its kind (observation/lifecycle).
    ///
    /// Bounded, never blocks the producer; drops are counted per queue under `DropPolicy`.
    pub fn publish_plugin_event(&mut self, event: Event) {
        self.plugin_host.publish(event);
    }

    /// Publish to a specific subscriber (lifecycle, owning plugin only).
    pub fn publish_plugin_event_to(
        &mut self,
        plugin_id: &PluginId,
        event: Event,
    ) -> Result<(), RuntimeError> {
        self.plugin_host
            .publish_to(plugin_id, event)
            .map_err(RuntimeError::from)
    }

    /// Drain a bounded batch for `plugin_id` + `kind` (FIFO, bounded by count/bytes).
    pub fn drain_plugin_events(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<Event>, RuntimeError> {
        self.plugin_host
            .drain_batch(plugin_id, kind, max_events, max_bytes)
            .map_err(RuntimeError::from)
    }

    /// Drain all queued events for `plugin_id` + `kind`.
    pub fn drain_plugin_events_all(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<Vec<Event>, RuntimeError> {
        self.plugin_host
            .drain(plugin_id, kind)
            .map_err(RuntimeError::from)
    }

    /// Drain side-queue observations (bounded host-mediated `HostObservation`s).
    pub fn drain_plugin_observations(&mut self) -> Vec<HostObservation> {
        self.plugin_host.drain_observations()
    }

    /// Drain side-queue observations up to `limit` (bounded batch).
    pub fn drain_plugin_observations_bounded(&mut self, limit: usize) -> Vec<HostObservation> {
        self.plugin_host.drain_observations_bounded(limit)
    }

    /// Push a [`HostObservation`] into the side queue (producer never blocks, bounded drops).
    ///
    /// Exposed for headless tests that drive observations without going through `handle_pty_bytes`.
    pub fn push_plugin_observation(&mut self, obs: HostObservation) {
        self.plugin_host.push_observation(obs);
    }

    /// Bridge all currently queued [`ColdEvent`]s into the side queue where a direct
    /// [`HostObservation`] mapping exists. This drains the `ColdQueue` and pushes
    /// corresponding observations without blocking; the side queue's bounded drop
    /// counter increments on overflow (visible via `plugin_side_dropped` for doctor).
    ///
    /// In steady state, `handle_pty_bytes` already bridges overlapping events automatically;
    /// this helper is for callers that have drained or synthesized cold events and want
    /// to observe them through the plugin host side queue headlessly.
    pub fn bridge_cold_to_side_queue(&mut self) {
        let drained = self.cold_queue.drain();
        for ev in drained {
            if let Some(obs) = cold_to_observation(&ev) {
                self.plugin_host.push_observation(obs);
            }
            // Keep the original cold event re-queued? No — draining is consuming.
            // For the cold+side dual accounting mode used by `handle_pty_bytes`,
            // we re-push the cold event so `cold_queue` remains observable.
            // But this drain-into-side is explicit; caller has already drained.
            // To preserve cold observability, we do not re-enqueue here — the
            // caller can decide to handle cold events separately. Documented honestly.
        }
    }

    // ── grant / command stubs (headless, no file I/O) ───────────────────

    /// Whether `capability` is granted for `plugin_id` under `manifest_hash`.
    #[must_use]
    pub fn is_capability_granted(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> bool {
        self.plugin_host
            .is_granted(plugin_id, manifest_hash, capability)
    }

    /// Insert a grant record (headless helper; persistence deferred).
    pub fn insert_grant(&mut self, record: GrantRecord) {
        self.plugin_host.insert_grant(record);
    }

    /// Revoke a capability or all grants for `plugin_id`.
    pub fn revoke_grant(
        &mut self,
        plugin_id: &PluginId,
        capability: Option<&CapabilityId>,
    ) -> Result<bitty_plugin_host::RevokeReport, RuntimeError> {
        self.plugin_host
            .revoke(plugin_id, capability)
            .map_err(RuntimeError::from)
    }

    /// Stub: check whether `plugin_id` may dispatch `command` under `manifest_hash` and `capability`.
    ///
    /// The full dispatch will run the command via the Lua VM with the plugin's grants;
    /// here we only intersect the requested capability with the grant store (deny-by-default,
    /// hash-bound). Returns `Ok(())` when granted, `Err(RuntimeError::Plugin)` otherwise.
    ///
    /// High-risk identifiers (`terminal.raw-read`, `ui.protocol-register`, etc.) are already
    /// flagged by `CapabilityId::is_high_risk`; revocation and workspace narrowing are
    /// enforced by the underlying `GrantStore`.
    pub fn check_command_grant(
        &self,
        plugin_id: &PluginId,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> Result<(), RuntimeError> {
        if self.is_capability_granted(plugin_id, manifest_hash, capability) {
            Ok(())
        } else {
            Err(RuntimeError::Plugin(format!(
                "command dispatch denied: plugin '{}' lacks capability '{capability}' for hash '{manifest_hash}' (deny-by-default)",
                plugin_id.as_str()
            )))
        }
    }

    /// Stub: grant-checked command dispatch.
    ///
    /// Validates that `qualified` is owned by `plugin_id` (via the registry) and that
    /// the required `capability` is granted. On success returns `Ok(())` as a
    /// placeholder for the future VM invocation; actual execution remains deferred.
    pub fn dispatch_command(
        &self,
        plugin_id: &PluginId,
        qualified: &QualifiedName,
        manifest_hash: &str,
        capability: &CapabilityId,
    ) -> Result<(), RuntimeError> {
        // Qualified name must be owned by this plugin (registry invariant: duplicates rejected at graph construction).
        let entry = self.plugin_host.registry().get(plugin_id).ok_or_else(|| {
            RuntimeError::Plugin(format!("plugin not found: '{}'", plugin_id.as_str()))
        })?;
        if !entry
            .commands
            .iter()
            .any(|c| c.as_str() == qualified.as_str())
        {
            return Err(RuntimeError::Plugin(format!(
                "command '{}' not owned by plugin '{}'",
                qualified.as_str(),
                plugin_id.as_str()
            )));
        }
        self.check_command_grant(plugin_id, manifest_hash, capability)
    }

    // ── interception routing (open points, cold-path synchronous) ─────────

    /// Accumulate interceptor decisions for a single user action (veto-wins, deterministic).
    ///
    /// This mirrors the RFC fail-open, veto-wins policy: a single `Veto` vetoes
    /// regardless of handler order; otherwise the action proceeds.
    #[must_use]
    pub fn accumulate_interceptions(decisions: &[InterceptionDecision]) -> InterceptionDecision {
        bitty_plugin_host::accumulate_interceptions(decisions)
    }

    /// Whether an intercepted action should proceed (`true`) or be vetoed (`false`) under fail-open.
    ///
    /// Timeouts are treated as abstention: the host proceeds without the plugin, records a
    /// violation, and disables the handler after repeated violations (threshold deferred to `OQ-014`).
    #[must_use]
    pub fn should_proceed_for_intercept(decision: InterceptionDecision, timed_out: bool) -> bool {
        bitty_plugin_host::should_proceed(decision, timed_out)
    }

    /// Convenience wrapper: `accumulate_interceptions` then `should_proceed`.
    #[must_use]
    pub fn should_proceed_after_interceptions(
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> bool {
        let acc = Self::accumulate_interceptions(decisions);
        Self::should_proceed_for_intercept(acc, timed_out)
    }

    /// Interception helper for `intercept.command-dispatch` (v1 of four points).
    ///
    /// Callers collect per-handler [`InterceptionDecision`]s (e.g. from future VM invocations)
    /// and pass them here; the host applies veto-wins and fail-open semantics.
    /// Reentrancy (a handler triggering another interception on the same thread) is rejected
    /// by the caller — nested interception is not defined behavior (RFC).
    #[must_use]
    pub fn intercept_command_dispatch(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.terminal-spawn` stub — same fail-open, veto-wins policy.
    #[must_use]
    pub fn intercept_terminal_spawn(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.paste` stub — bounded metadata path (no clipboard text without `clipboard.read`).
    #[must_use]
    pub fn intercept_paste(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    /// `intercept.open-url` stub.
    #[must_use]
    pub fn intercept_open_url(decisions: &[InterceptionDecision], timed_out: bool) -> bool {
        Self::should_proceed_after_interceptions(decisions, timed_out)
    }

    // ------------------------------------------------------------------
    // Layout + Focus ownership (CTX-0023)
    // ------------------------------------------------------------------

    /// Immutable view of the owned layout tree.
    #[must_use]
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }

    /// Mutable view of the owned layout tree.
    #[must_use]
    pub fn layout_mut(&mut self) -> &mut LayoutNode {
        &mut self.layout
    }

    /// Replaces the owned layout tree.
    ///
    /// The new tree's leaf `View`s are kept as provided; `tick` will reflow
    /// them into the current container on the next frame. Focus is retained
    /// when the focused `ViewId` still exists, otherwise it moves to the
    /// first leaf (if any) or clears.
    pub fn set_layout(&mut self, layout: LayoutNode) {
        self.layout = layout;
        let leaf_ids = self.layout.leaf_ids();
        if leaf_ids.is_empty() {
            self.focus.clear();
        } else if let Some(focused) = self.focus.focused() {
            if !leaf_ids.contains(&focused) {
                self.focus.set(leaf_ids[0]);
            }
        } else {
            self.focus.set(leaf_ids[0]);
        }
        self.pending_full_redraw = true;
    }

    /// Owned focus state.
    #[must_use]
    pub fn focus(&self) -> &Focus {
        &self.focus
    }

    /// Mutable focus state.
    #[must_use]
    pub fn focus_mut(&mut self) -> &mut Focus {
        &mut self.focus
    }

    /// Currently focused view, if any.
    #[must_use]
    pub fn focused_view(&self) -> Option<ViewId> {
        self.focus.focused()
    }

    /// Sets focus to `id` when it exists in the current layout; otherwise
    /// leaves focus unchanged and returns `false`.
    pub fn set_focus(&mut self, id: ViewId) -> bool {
        if self.layout.leaf_ids().contains(&id) {
            self.focus.set(id);
            true
        } else {
            false
        }
    }

    /// Moves focus in `dir` using the layout's deterministic adjacency.
    ///
    /// Returns the new focused view (if any) and updates internal focus.
    pub fn move_focus(&mut self, dir: FocusDirection) -> Option<ViewId> {
        let next = self.focus.advance(&self.layout, self.container, dir);
        if let Some(id) = next {
            self.focus.set(id);
        }
        next
    }

    /// Container rect (cell coordinates) that the layout is reflowed into.
    #[must_use]
    pub fn container(&self) -> UiRect {
        self.container
    }

    /// Sets the container rect directly (cell coordinates). The container is
    /// also updated automatically by `handle_resize` via pixel-to-cell
    /// conversion; this setter exists for headless tests that drive layout
    /// without a physical surface.
    pub fn set_container(&mut self, rect: UiRect) {
        self.container = rect;
        self.pending_full_redraw = true;
    }

    /// Current leaf allocations `(ViewId, Rect)` in deterministic depth-first
    /// order, computed from the last reflowed layout or the current container
    /// without mutating the tree (pure `LayoutNode::layout`).
    #[must_use]
    pub fn layout_allocations(&self) -> Vec<(ViewId, UiRect)> {
        self.layout.layout(self.container)
    }

    /// Leaf count of the current layout.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        self.layout.leaf_count()
    }

    /// Reflows the layout into the current container, mutating each leaf
    /// `View`'s `cols`/`rows`/`origin` to match its allocation. Returns the
    /// allocations for inspection. Deterministic over the same layout and
    /// container.
    pub fn reflow_layout(&mut self) -> Vec<(ViewId, UiRect)> {
        self.layout.reflow(self.container);
        self.layout.layout(self.container)
    }

    /// Spawns `program` inside a PTY sized to the current grid, storing the
    /// child handle. The program is taken as a direct argv[0] without shell
    /// interpolation (P0 security posture).
    ///
    /// Replaces any previously spawned child: the old `Pty` is dropped, which
    /// kills and reaps its child without leaking a zombie. The output side
    /// is pumped into a bounded channel (`READ_CHUNK_SIZE` ×
    /// `CHANNEL_CAPACITY_CHUNKS` = 128 KiB) so backpressure is end-to-end;
    /// see [`poll_pty`] for the non-blocking drain that feeds
    /// [`handle_pty_bytes`].
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `program` is blank;
    /// [`RuntimeError::Pty`] when the platform reports spawn failure
    /// (`Unsupported` on Windows before the ConPTY slice, `Upstream` or
    /// `Io` elsewhere).
    pub fn spawn_shell(&mut self, program: &str) -> Result<(), RuntimeError> {
        self.spawn_shell_with_args(program, &[])
    }

    /// Spawns `program` with additional `args` inside a PTY sized to the
    /// current grid.
    ///
    /// Direct argv exec, no shell interpolation: `program` plus `args` are
    /// passed verbatim to the platform exec path. For a shell echo, pass
    /// `program = "/bin/sh"` and `args = &["-c", "echo hello"]`. Bounded
    /// backpressure and lifecycle are identical to [`spawn_shell`].
    pub fn spawn_shell_with_args(
        &mut self,
        program: &str,
        args: &[&str],
    ) -> Result<(), RuntimeError> {
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        let cols = self.cols.min(u16::MAX as usize) as u16;
        let rows = self.rows.min(u16::MAX as usize) as u16;
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // Replace any previously spawned child (drop kills old).
        self.pty = Some(pty);
        self.pty_reader = Some(reader);
        self.pty_writer = Some(writer);
        // Clear pending input on new shell: fresh session, no stale keystrokes.
        self.pending_input.clear();
        self.pending_input_dropped = 0;
        Ok(())
    }

    /// Whether a PTY child is currently owned.
    #[must_use]
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// Whether a PTY reader channel is currently owned (implies [`has_pty`]).
    #[must_use]
    pub fn has_pty_reader(&self) -> bool {
        self.pty_reader.is_some()
    }

    /// Process id of the child, when available.
    #[must_use]
    pub fn pty_pid(&self) -> Option<u32> {
        self.pty.as_ref().and_then(|p| p.pid())
    }

    /// Current PTY size as known by the kernel, if a PTY is owned.
    pub fn pty_size(&self) -> Option<(u16, u16)> {
        self.pty.as_ref().and_then(|p| p.size().ok())
    }

    /// Takes exclusive ownership of the bounded PTY output reader, if present.
    ///
    /// The caller becomes responsible for draining the channel without blocking
    /// the runtime thread and for joining the pump on EOF. After this call
    /// [`poll_pty`] will return `0` because the channel no longer belongs to
    /// the runtime; most embedders should prefer [`poll_pty`] instead.
    pub fn take_pty_reader(&mut self) -> Option<PtyReader> {
        self.pty_reader.take()
    }

    /// Non-blocking drain of the bounded PTY output channel into
    /// [`handle_pty_bytes`].
    ///
    /// When a consumer stalls, the bounded channel (`CHANNEL_CAPACITY_CHUNKS` ×
    /// `READ_CHUNK_SIZE` = 128 KiB) fills, the pump blocks, the kernel PTY
    /// buffer fills, and the child's writes block — end-to-end backpressure
    /// with zero data loss and zero unbounded memory growth. This method is
    /// the consumer side: it drains all immediately available chunks without
    /// blocking, feeding each through the VT parser and terminal state.
    ///
    /// Returns the number of chunks drained. `0` means either no PTY, no data
    /// available yet, or EOF has been reached and the queue drained. Headless
    /// tests that never called [`spawn_shell`] get `0` without error, so the
    /// same binary works headlessly (synthetic `handle_pty_bytes`) and with a
    /// real PTY (live `poll_pty`).
    pub fn poll_pty(&mut self) -> usize {
        // Collect without holding an immutable borrow across the mutable
        // `handle_pty_bytes` call (borrow checker).
        let chunks: Vec<Vec<u8>> = {
            let Some(reader) = self.pty_reader.as_ref() else {
                return 0;
            };
            let mut out = Vec::new();
            while out.len() < 1024 {
                match reader.try_recv() {
                    Some(chunk) => {
                        debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                        out.push(chunk);
                    }
                    None => break,
                }
            }
            out
        };
        let drained = chunks.len();
        for chunk in chunks {
            self.handle_pty_bytes(&chunk);
        }
        drained
    }

    /// Blocking drain with a timeout, returning the number of chunks drained.
    ///
    /// Blocks at most `timeout` for the first chunk; once data is flowing it
    /// drains all immediately available chunks without further blocking. Useful
    /// for tests that need to wait for a shell echo. Returns `0` on timeout
    /// or EOF.
    pub fn poll_pty_timeout(&mut self, timeout: std::time::Duration) -> usize {
        let first: Option<Vec<u8>> = {
            let Some(reader) = self.pty_reader.as_ref() else {
                return 0;
            };
            match reader.recv_timeout(timeout) {
                Ok(Some(chunk)) => Some(chunk),
                Ok(None) | Err(_) => None,
            }
        };
        match first {
            Some(chunk) => {
                self.handle_pty_bytes(&chunk);
                1 + self.poll_pty()
            }
            None => 0,
        }
    }

    /// Feeds raw PTY bytes through the parser into terminal state, enqueuing
    /// bounded cold-path observations derived from the actions.
    ///
    /// The byte stream may be split arbitrarily; splitting the same bytes
    /// differently yields the same action sequence (deterministic replay
    /// contract). Malformed or hostile sequences are bounded and never panic.
    ///
    /// Bridging (ADR-0003 rule 4): every [`ColdEvent`] that has a direct
    /// [`HostObservation`] mapping is also pushed into the bounded
    /// [`PluginHost`] side queue without blocking the hot path. When the side
    /// queue is full the oldest observation is dropped and
    /// [`Self::plugin_side_dropped`] increments (counted for `bitty plugin doctor`).
    pub fn handle_pty_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut actions: Vec<TerminalAction> = Vec::new();
        self.parser.advance(bytes, |action| actions.push(action));
        for action in actions {
            // Map terminal actions to cold-path observations before state
            // mutation where the payload lives on the action itself; state
            // is the authority for derived values like title after mutation.
            let pre_event = match &action {
                TerminalAction::OscTitle { text } => {
                    Some(ColdEvent::TitleChanged(text.as_str().to_owned()))
                }
                TerminalAction::OscCwd { url } => {
                    Some(ColdEvent::CwdChanged(url.as_str().to_owned()))
                }
                TerminalAction::OscPromptMark { kind } => Some(ColdEvent::ZoneMarked(*kind)),
                TerminalAction::OscHyperlink { link } => Some(ColdEvent::HyperlinkChanged(
                    link.as_ref().map(|h| h.uri.as_str().to_owned()),
                )),
                TerminalAction::SetMode { mode, enabled } => Some(ColdEvent::ModeChanged {
                    mode: *mode,
                    enabled: *enabled,
                }),
                TerminalAction::Unknown(seq) => Some(ColdEvent::UnknownSequence(seq.kind)),
                TerminalAction::OscUnknown { .. } => {
                    Some(ColdEvent::UnknownSequence(SequenceKind::Csi))
                }
                TerminalAction::PrintControl(ctrl) if ctrl.0 == 0x07 => Some(ColdEvent::Bell),
                _ => None,
            };
            if let Some(ev) = pre_event {
                // Cold queue: bounded, drop-oldest, never blocks.
                self.cold_queue.push(ev.clone());
                // Side queue bridging: bounded, same non-blocking guarantee (ADR-0003 rule 4).
                if let Some(obs) = cold_to_observation(&ev) {
                    self.plugin_host.push_observation(obs);
                }
            }
            let damage = self.state.apply(&action);
            if !damage.regions.is_empty() {
                let generation = damage.generation;
                self.cold_queue.push(ColdEvent::Damage { generation });
                // Bridge damage generation as well.
                self.plugin_host
                    .push_observation(HostObservation::Damage { generation });
            }
        }
    }

    /// Handles a physical-pixel resize: recomputes the grid size from the
    /// configured cell metrics, reconfigures the software surface, updates
    /// the layout container, reflows leaf views, and resizes the PTY when
    /// present. Grid memory resize (the state reflow algorithm) is deferred
    /// per the terminal-state-rfc open item, so the state grid stays at its
    /// current dimensions while the surface and PTY reflect the new window
    /// size honestly. Zero-sized extents are skipped (minimized/occluded
    /// windows) per the `map_resize_to_surface_extent` contract.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Render`] when headless reconfiguration rejects the
    /// extent; [`RuntimeError::Pty`] when the PTY resize fails.
    pub fn handle_resize(&mut self, size: PhysicalSize) -> Result<(), RuntimeError> {
        // Software surface: zero-sized skips reconfiguration, exactly like the
        // GPU path (map_resize_to_surface_extent returns None).
        if bitty_platform::map_resize_to_surface_extent(size).is_none() {
            return Ok(());
        }
        // Recompute logical grid size for the PTY and stored config. The
        // state grid itself cannot resize yet (singular reflow algorithm
        // deferred), so we keep the state's 80x24 until that lands and hold
        // the logical size separately for PTY and surface bookkeeping.
        let (new_cols, new_rows) = self.config.grid_from_pixels(size);
        self.cols = new_cols;
        self.rows = new_rows;
        self.container = default_container(new_cols, new_rows);
        // Reflow leaf views to new container allocations before the next tick
        // (tick will also reflow, but doing it here keeps `layout()` view of
        // leaf sizes consistent immediately after resize for callers that
        // query without ticking).
        self.layout.reflow(self.container);
        self.surface
            .headless_resize(size)
            .map_err(RuntimeError::from)?;
        if let Some(pty) = self.pty.as_mut() {
            let cols = self.cols.min(u16::MAX as usize) as u16;
            let rows = self.rows.min(u16::MAX as usize) as u16;
            pty.resize(cols, rows).map_err(RuntimeError::from)?;
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Handles one platform event, returning `true` when the event asks the
    /// application loop to exit (window close requested or `Exiting` phase).
    ///
    /// Resize events are routed through [`Self::handle_resize`]; keyboard
    /// input is encoded via the legacy xterm table (CTX-0057) and routed to
    /// the PTY writer when live, otherwise buffered as bounded pending input
    /// for headless observation. Other window events currently return `false`
    /// without side effects so the hot path stays deterministic.
    pub fn handle_platform_event(&mut self, event: PlatformEvent) -> bool {
        match event {
            PlatformEvent::Window { window_id: _, kind } => match kind {
                WindowEventKind::Resized(size) => {
                    let _ = self.handle_resize(size);
                    false
                }
                WindowEventKind::ScaleFactorChanged(_) => {
                    // DPI factor alone carries no new physical size in this
                    // seam: a `Resized` event with the refresh-computed
                    // extent follows and takes precedence. Keeping the
                    // headless seam single-threaded, we wait for that event
                    // rather than fabricating a size here. Documented as a
                    // deferred piece of the DPI refresh hook in
                    // `bitty_platform::SurfaceTarget::logical_to_physical`.
                    false
                }
                WindowEventKind::CloseRequested | WindowEventKind::Closed => true,
                WindowEventKind::RedrawRequested => {
                    // The embedder will call `tick` on `AboutToWait`; we do
                    // not present eagerly here so frame-on-demand stays
                    // honest (no periodic wakeups when idle).
                    false
                }
                WindowEventKind::KeyboardInput(key) => {
                    let _ = self.handle_key_event(key);
                    false
                }
                _ => false,
            },
            PlatformEvent::Exiting => true,
            _ => false,
        }
    }

    /// Drains queued PTY replies (device-status responses) without I/O.
    ///
    /// Terminal state synthesizes replies into a bounded queue; the runtime
    /// exposes them here so the embedder can write them back to the PTY
    /// master via `PtyWriter`. No upstream type is exposed.
    pub fn take_replies(&mut self) -> Vec<Box<[u8]>> {
        self.state.take_replies()
    }

    /// Whether any reply was dropped due to the cap since the last drain.
    #[must_use]
    pub fn replies_overflowed(&self) -> bool {
        self.state.replies_overflowed()
    }

    /// Plans, records, and presents one frame when damage exists, returning
    /// [`PresentStats`] on a presented frame and `None` when idle.
    ///
    /// Frame-on-demand: zero damage (including pure scrollback damage that
    /// adds no pixels on this viewport) presents nothing and burns no CPU
    /// beyond the damage check — the idle resource budget (PB-7, ≤ 1% CPU)
    /// depends on this property. The first frame after creation or resize
    /// forces a full redraw.
    ///
    /// Multi-pane: the layout tree is reflowed into the current container;
    /// each leaf `View`'s `cols`/`rows`/`origin` are updated via
    /// `LayoutNode::reflow`. Then each leaf is rendered: a viewport snapshot
    /// sized to the leaf's dimensions is built from the shared `State`
    /// snapshot (headless seam, no GPU/window required), rendered through the
    /// shared `GridRenderer` with a full-damage hint, and its `DrawList`
    /// translated to the leaf's pixel origin. The per-leaf `DrawList`s are
    /// combined and presented once via `Surface::headless_present`.
    ///
    /// The software seam composites `DrawList + Atlas` onto an owned RGBA
    /// buffer via `Surface::headless_present`; no display server or adapter
    /// is touched. Real GPU present remains env-gated (`BITTY_RENDER_GPU_TESTS=1`)
    /// and is not available on headless CI — `is_headless` is `true` for
    /// every present this method emits today.
    pub fn tick(&mut self) -> Option<PresentStats> {
        // Reflow layout tree into container before rendering so leaf Views
        // carry deterministic origins/sizes for this frame. This is headless
        // and deterministic: same layout + container always yields same
        // allocations.
        self.layout.reflow(self.container);

        let snapshot = self.state.snapshot();
        let pending_full = self.pending_full_redraw;
        let last = self.last_presented_generation;
        let current_gen = snapshot.generation;

        // Frame-on-demand: no new generation and no forced redraw -> idle.
        if !pending_full && current_gen == last && last != u64::MAX {
            return None;
        }

        // Collect allocations deterministically; empty layout -> idle (no leaf to present).
        let allocations = self.layout.layout(self.container);
        if allocations.is_empty() {
            self.last_presented_generation = current_gen;
            self.pending_full_redraw = false;
            return None;
        }

        // Build the combined DrawList by rendering each leaf's viewport.
        let mut combined_fills = Vec::new();
        let mut combined_glyphs = Vec::new();
        let mut any_needs_draw = false;

        // For damage, we treat any new generation or pending_full as full
        // per leaf (over-damage safe, deterministic). If generation gap is
        // large and regions empty, also full. Otherwise still full for
        // correctness with viewport slicing.
        let use_full = pending_full
            || last == u64::MAX
            || {
                let gap = current_gen.saturating_sub(last);
                let regions = self.state.damage_since(last);
                gap > bitty_term_state::damage::DAMAGE_HISTORY_BATCHES as u64 && regions.is_empty()
            }
            || {
                let regions = self.state.damage_since(last);
                !regions.is_empty() || pending_full
            };

        for (_view_id, rect) in &allocations {
            if rect.is_empty() {
                continue;
            }
            let view_snapshot = viewport_snapshot(&snapshot, rect.width, rect.height);
            let damage = if use_full || pending_full || last == u64::MAX {
                Damage {
                    generation: current_gen,
                    regions: vec![DamagedRegion::Grid(DamageRect::full(
                        view_snapshot.height as u16,
                        view_snapshot.width as u16,
                    ))]
                    .into_boxed_slice(),
                }
            } else {
                // Incremental path: clip damage_since to viewport bounds.
                // For this slice we still produce a full per-leaf damage when
                // any damage exists (over-damage safe), keeping determinism.
                let regions = self.state.damage_since(last);
                if regions.is_empty() {
                    Damage {
                        generation: current_gen,
                        regions: Box::new([]),
                    }
                } else {
                    Damage {
                        generation: current_gen,
                        regions: vec![DamagedRegion::Grid(DamageRect::full(
                            view_snapshot.height as u16,
                            view_snapshot.width as u16,
                        ))]
                        .into_boxed_slice(),
                    }
                }
            };

            if damage.regions.is_empty() {
                continue;
            }

            let list = match self.renderer.render(&view_snapshot, &damage) {
                Ok(list) => list,
                Err(_) => continue,
            };
            if !list.needs_draw() {
                continue;
            }
            any_needs_draw = true;

            let origin_px_x = rect.x as i32 * self.config.cell_width as i32;
            let origin_px_y = rect.y as i32 * self.config.cell_height as i32;
            for mut fill in list.fills {
                fill.rect.x += origin_px_x;
                fill.rect.y += origin_px_y;
                combined_fills.push(fill);
            }
            for mut glyph in list.glyphs {
                glyph.dest[0] += origin_px_x;
                glyph.dest[1] += origin_px_y;
                combined_glyphs.push(glyph);
            }
        }

        self.pending_full_redraw = false;

        if !any_needs_draw && combined_fills.is_empty() && combined_glyphs.is_empty() {
            // Check if we had pending_full but produced no draws (e.g., all zero rects) -> still idle
            // But ensure generation advances for idle detection.
            self.last_presented_generation = current_gen;
            return None;
        }

        // Synthesize a DrawList for the combined frame. Plan is not used by
        // headless_present beyond fill/glyph counts, so we create a minimal
        // plan that reports needs_draw == true when we have content.
        let combined_list = {
            // We need a FramePlan; construct via a dummy damage descriptor that
            // indicates full. Simplest: reuse empty plan but set dirty_rects
            // to surface extent so needs_draw is true. Instead we construct a
            // DrawList manually with a plan that has needs_draw true.
            // The easiest is to create a FramePlan::default-like but we don't
            // have that. So we synthesize via render's FramePlan by creating
            // a dummy DrawList from first leaf and replacing its fills/glyphs.
            // Instead, construct a DrawList with a plan that has one dirty rect.
            // Look at FramePlan structure: we can get it from rendering a full
            // snapshot once and reusing its plan.
            // For minimal, we will create a plan via the renderer's internal
            // but we can just create an empty plan with needs_draw = true by
            // using a helper: we know DrawList::needs_draw checks fills/glyphs,
            // not plan alone. So plan can be empty as headless_present doesn't
            // check plan.
            // Let's create a dummy plan via unsafe uninitialized? Better to just
            // reuse a plan from first allocation's render if available.
            // Instead, we will construct a DrawList with a plan that we know
            // will have dirty_rects covering the surface. We can synthesize by
            // directly constructing FramePlan via its public fields if any.
            // Check FramePlan fields - read its definition.
            // As a shortcut, we will create a DrawList with plan from a full
            // render of the viewport_snapshot for the container size.
            // But simpler: we can just create a DrawList with plan that has
            // needs_draw true by fabricating via bitty_render's test helper?
            // Most straightforward: create a DrawList with an empty plan that
            // we override to have a dirty rect, but we don't have constructor.
            // Workaround: create a DrawList via renderer.render of a 1x1 snapshot
            // and then replace its fills/glyphs.
            let tmp_snap = viewport_snapshot(&snapshot, 1, 1);
            let tmp_damage = Damage {
                generation: current_gen,
                regions: vec![DamagedRegion::Grid(DamageRect::full(1, 1))].into_boxed_slice(),
            };
            let mut tmp_list = self
                .renderer
                .render(&tmp_snap, &tmp_damage)
                .unwrap_or(DrawList {
                    generation: current_gen,
                    plan: FramePlan {
                        dirty_rects: Vec::new(),
                        extent: bitty_render::geometry::ExtentPx::new(0, 0),
                        mode: FrameMode::Clean,
                    },
                    fills: Vec::new(),
                    glyphs: Vec::new(),
                });
            // Now replace fills/glyphs with combined, and set plan to indicate full
            tmp_list.generation = current_gen;
            tmp_list.fills = combined_fills;
            tmp_list.glyphs = combined_glyphs;
            // Ensure plan indicates dirty if we have content; set a dummy dirty rect
            if tmp_list.fills.is_empty() && tmp_list.glyphs.is_empty() {
                tmp_list.plan.dirty_rects = Vec::new();
            } else if tmp_list.plan.dirty_rects.is_empty() {
                // Fabricate one dirty rect covering the surface extent so
                // needs_draw logic that might inspect plan still sees work.
                // The DrawList::needs_draw checks fills/glyphs, so this is
                // not strictly needed, but we set it for completeness.
                tmp_list.plan.dirty_rects = vec![bitty_render::geometry::RectPx::new(
                    0,
                    0,
                    self.surface.extent().map(|e| e.width()).unwrap_or(0),
                    self.surface.extent().map(|e| e.height()).unwrap_or(0),
                )];
                tmp_list.plan.mode = FrameMode::Full;
                tmp_list.plan.extent = bitty_render::geometry::ExtentPx::new(
                    self.surface.extent().map(|e| e.width()).unwrap_or(0),
                    self.surface.extent().map(|e| e.height()).unwrap_or(0),
                );
            }
            tmp_list
        };

        if !combined_list.needs_draw() {
            self.last_presented_generation = current_gen;
            return None;
        }

        let atlas_texels = self.renderer.atlas_texels().to_vec();
        let dims = self.renderer.atlas_dims();
        let stats = match self
            .surface
            .headless_present(&combined_list, Some((&atlas_texels, dims)))
        {
            Ok(stats) => stats,
            Err(_) => return None,
        };
        self.last_presented_generation = current_gen;
        Some(PresentStats {
            frame: stats.frame,
            fills: stats.fills,
            glyphs: stats.glyphs,
            headless: stats.headless,
            generation: current_gen,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ui::{Rect as UiRect, SplitAxis};

    fn make_runtime() -> Runtime {
        Runtime::with_defaults().expect("defaults must build")
    }

    #[test]
    fn tick_is_idle_when_no_damage() {
        let mut rt = make_runtime();
        let first = rt.tick().expect("first tick must present full redraw");
        assert!(first.headless);
        assert_eq!(
            rt.tick(),
            None,
            "second tick with no new bytes must be idle"
        );
    }

    #[test]
    fn handle_pty_bytes_flow_reaches_render() {
        let mut rt = make_runtime();
        assert!(rt.tick().is_some());
        rt.handle_pty_bytes(b"hello ");
        let stats = rt.tick().expect("damage from bytes must present");
        assert!(stats.glyphs > 0);
        assert_eq!(rt.tick(), None, "must return to idle after present");
    }

    #[test]
    fn handle_resize_reconfigures_surface_and_keeps_grid_pending_full_redraw() {
        let mut rt = make_runtime();
        let before = rt.surface_extent().expect("surface must have extent");
        assert_eq!(before, RuntimeConfig::default().pixel_extent());
        rt.handle_resize(PhysicalSize::new(800, 600))
            .expect("valid resize");
        assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
        assert!(rt.tick().is_some(), "resize forces full redraw");
    }

    #[test]
    fn zero_resize_is_skipped_honestly() {
        let mut rt = make_runtime();
        rt.handle_resize(PhysicalSize::new(0, 0))
            .expect("zero resize must not error");
        assert_eq!(
            rt.surface_extent(),
            Some(RuntimeConfig::default().pixel_extent()),
            "zero resize must not reconfigure"
        );
    }

    #[test]
    fn cold_queue_is_bounded_and_observable() {
        let cfg = RuntimeConfig {
            cold_queue_capacity: 2,
            ..RuntimeConfig::default()
        };
        let mut rt = Runtime::new(cfg).expect("must build");
        rt.handle_pty_bytes(b"\x1b]0;first\x07");
        rt.handle_pty_bytes(b"\x1b]0;second\x07");
        rt.handle_pty_bytes(b"\x1b]0;third\x07");
        assert_eq!(rt.cold_queue_len(), 2);
        assert!(rt.cold_queue_dropped() > 0);
        let drained: Vec<_> = rt.drain_cold_events();
        assert_eq!(drained.len(), 2);
        assert_eq!(rt.cold_queue_len(), 0);
    }

    #[test]
    fn handle_platform_event_close_semantics() {
        let mut rt = make_runtime();
        let close = rt.handle_platform_event(PlatformEvent::Exiting);
        assert!(close);
        assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
        assert!(!rt.handle_platform_event(PlatformEvent::AboutToWait));
        assert!(!rt.handle_platform_event(PlatformEvent::Suspended));
    }

    #[test]
    fn handle_platform_event_resize_via_handle_resize() {
        let mut rt = make_runtime();
        rt.handle_resize(PhysicalSize::new(320, 240))
            .expect("valid resize");
        assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(320, 240)));
        assert!(rt.tick().is_some(), "resize forces full redraw");
    }

    #[test]
    fn spawn_shell_blank_program_rejected_without_touching_pty() {
        let mut rt = make_runtime();
        assert!(rt.spawn_shell("").is_err());
        assert!(rt.spawn_shell("   ").is_err());
    }

    // Ensures missing `allow` does not leak `dead_code` on the Window target.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_build_still_compiles_with_queue_and_tick() {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"hi");
        let _ = rt.tick();
        assert!(rt.is_headless());
    }

    // ------------------------------------------------------------------
    // CTX-0023: LayoutNode wiring tests
    // ------------------------------------------------------------------

    #[test]
    fn default_layout_is_single_leaf_and_focused() {
        let rt = make_runtime();
        assert_eq!(rt.leaf_count(), 1);
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        assert_eq!(rt.container(), UiRect::new(0, 0, 80, 24));
        let allocs = rt.layout_allocations();
        assert_eq!(allocs.len(), 1);
        assert_eq!(allocs[0].0, ViewId::new(1));
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
    }

    #[test]
    fn set_layout_replaces_tree_and_updates_focus() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(10), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(20), 40, 24)),
        );
        rt.set_layout(split);
        assert_eq!(rt.leaf_count(), 2);
        // Focus should move to first leaf of new tree since old focus (1) no longer exists
        assert_eq!(rt.focused_view(), Some(ViewId::new(10)));
        let ids = rt.layout().leaf_ids();
        assert_eq!(ids, vec![ViewId::new(10), ViewId::new(20)]);
    }

    #[test]
    fn set_layout_retains_focus_when_still_present() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt.set_layout(split);
        // Focused view 1 still exists, should be retained
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        rt.set_focus(ViewId::new(2));
        assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
        // Replace with another tree containing 2 but not 1 -> focus moves to first leaf
        let split2 = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
        );
        rt.set_layout(split2);
        assert_eq!(rt.focused_view(), Some(ViewId::new(2))); // 2 still present, retained
        let split3 = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(4), 40, 24)),
        );
        rt.set_layout(split3);
        assert_eq!(rt.focused_view(), Some(ViewId::new(3))); // 2 gone, first leaf becomes focused
    }

    #[test]
    fn reflow_updates_view_origins_and_sizes() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
            LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
        );
        rt.set_layout(split);
        rt.set_container(UiRect::new(0, 0, 80, 24));
        let allocs = rt.reflow_layout();
        assert_eq!(allocs.len(), 2);
        // Horizontal split 80 cols -> 40 each
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 40, 24));
        assert_eq!(allocs[1].1, UiRect::new(40, 0, 40, 24));
        // Views themselves must have been reflowed
        let v1 = rt.layout().find_leaf(ViewId::new(1)).unwrap();
        assert_eq!(v1.origin(), bitty_ui::Point::new(0, 0));
        assert_eq!(v1.cols(), 40);
        assert_eq!(v1.rows(), 24);
        let v2 = rt.layout().find_leaf(ViewId::new(2)).unwrap();
        assert_eq!(v2.origin(), bitty_ui::Point::new(40, 0));
        assert_eq!(v2.cols(), 40);
    }

    #[test]
    fn focus_movement_next_prev_and_spatial() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt.set_layout(split);
        rt.set_container(UiRect::new(0, 0, 80, 24));
        rt.reflow_layout();
        assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
        let next = rt.move_focus(FocusDirection::Next);
        assert_eq!(next, Some(ViewId::new(2)));
        assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
        let prev = rt.move_focus(FocusDirection::Prev);
        assert_eq!(prev, Some(ViewId::new(1)));
        // Spatial right from left pane goes to right pane
        let right = rt.move_focus(FocusDirection::Right);
        assert_eq!(right, Some(ViewId::new(2)));
        let left = rt.move_focus(FocusDirection::Left);
        assert_eq!(left, Some(ViewId::new(1)));
    }

    #[test]
    fn tick_with_split_composites_both_leaves_headlessly() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt.set_layout(split);
        rt.handle_pty_bytes(b"hello");
        let stats = rt.tick().expect("split tick must present");
        assert!(stats.headless);
        assert!(stats.fills > 0);
        // Both leaves produce fills; total fills should be > single leaf.
        // Single leaf for 80x24 produces one fill per cell visited; split
        // produces per-leaf fills translated. So we expect fills roughly double
        // but at least more than one leaf's minimal.
        assert!(
            stats.fills >= 2,
            "split must composite at least two leaf fills"
        );
        let rgba = rt.headless_rgba().expect("rgba after split");
        assert!(!rgba.is_empty());
        // Deterministic: second runtime with same layout and bytes must be identical
        let mut rt2 = make_runtime();
        let split2 = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt2.set_layout(split2);
        rt2.handle_pty_bytes(b"hello");
        let stats2 = rt2.tick().expect("second split must present");
        let rgba2 = rt2.headless_rgba().expect("second rgba");
        assert_eq!(stats.fills, stats2.fills);
        assert_eq!(stats.glyphs, stats2.glyphs);
        assert_eq!(rgba, rgba2, "deterministic split composition");
    }

    #[test]
    fn tick_with_stack_and_overlay_prove_composition() {
        let mut rt = make_runtime();
        // Stack: two children full size (second on top)
        let stack = LayoutNode::stack(vec![
            LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
        ]);
        rt.set_layout(stack);
        rt.handle_pty_bytes(b"stack");
        let stats = rt.tick().expect("stack tick must present");
        assert!(stats.headless);
        assert!(stats.fills > 0);
        let rgba_stack = rt.headless_rgba().expect("stack rgba").clone();

        // Overlay: base plus floating overlay
        let overlay = LayoutNode::overlay(
            LayoutNode::leaf(View::new(ViewId::new(10), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(20), 20, 10)),
            UiRect::new(5, 5, 20, 10),
        );
        let mut rt2 = make_runtime();
        rt2.set_layout(overlay);
        rt2.handle_pty_bytes(b"overlay");
        let stats2 = rt2.tick().expect("overlay tick must present");
        assert!(stats2.headless);
        let rgba_overlay = rt2.headless_rgba().expect("overlay rgba");
        assert_ne!(
            rgba_stack, rgba_overlay,
            "different compositions produce different pixels"
        );
        // Overlay must still be deterministic
        let mut rt3 = make_runtime();
        let overlay2 = LayoutNode::overlay(
            LayoutNode::leaf(View::new(ViewId::new(10), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(20), 20, 10)),
            UiRect::new(5, 5, 20, 10),
        );
        rt3.set_layout(overlay2);
        rt3.handle_pty_bytes(b"overlay");
        rt3.tick().expect("overlay replay");
        assert_eq!(rgba_overlay, rt3.headless_rgba().unwrap());
    }

    #[test]
    fn tick_with_empty_stack_is_idle() {
        let mut rt = make_runtime();
        rt.set_layout(LayoutNode::stack(vec![]));
        assert_eq!(rt.leaf_count(), 0);
        assert_eq!(rt.focused_view(), None);
        // Even with pending full redraw, empty layout has no leaf to present
        assert_eq!(rt.tick(), None);
    }

    #[test]
    fn deterministic_layout_same_tree_same_container() {
        let mut rt = make_runtime();
        let tree = LayoutNode::split(
            SplitAxis::Vertical,
            0.3,
            LayoutNode::leaf(View::new(ViewId::new(5), 80, 10)),
            LayoutNode::split(
                SplitAxis::Horizontal,
                0.7,
                LayoutNode::leaf(View::new(ViewId::new(6), 40, 14)),
                LayoutNode::leaf(View::new(ViewId::new(7), 40, 14)),
            ),
        );
        rt.set_layout(tree.clone());
        rt.set_container(UiRect::new(0, 0, 100, 40));
        let a1 = rt.reflow_layout();
        let mut rt2 = make_runtime();
        rt2.set_layout(tree);
        rt2.set_container(UiRect::new(0, 0, 100, 40));
        let a2 = rt2.reflow_layout();
        assert_eq!(a1, a2, "layout must be deterministic");
    }

    #[test]
    fn handle_resize_updates_container_and_reflows() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        );
        rt.set_layout(split);
        // Resize to 800x600 pixels with default cell 8x16 => 100x37 cells
        rt.handle_resize(PhysicalSize::new(800, 600))
            .expect("resize");
        assert_eq!(rt.container(), UiRect::new(0, 0, 100, 37));
        let allocs = rt.layout_allocations();
        // Horizontal split of 100 -> 50 each
        assert_eq!(allocs[0].1.width, 50);
        assert_eq!(allocs[1].1.width, 50);
        assert_eq!(allocs[0].1.height, 37);
    }

    #[test]
    fn set_container_headless_seam_without_gpu() {
        let mut rt = make_runtime();
        let split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
            LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
        );
        rt.set_layout(split);
        // Drive layout math headlessly without surface resize
        rt.set_container(UiRect::new(0, 0, 60, 20));
        let allocs = rt.layout_allocations();
        assert_eq!(allocs[0].1, UiRect::new(0, 0, 30, 20));
        assert_eq!(allocs[1].1, UiRect::new(30, 0, 30, 20));
        // Tick must still present via headless seam (surface is still 640x384,
        // but layout container is 60x20 cells; rendering will still composite
        // correctly, and no window is required).
        rt.handle_pty_bytes(b"headless");
        assert!(rt.tick().is_some());
        assert!(rt.is_headless());
        assert!(rt.headless_rgba().is_some());
    }

    // ------------------------------------------------------------------
    // CTX-0027: plugin-host wiring tests (headless, no Lua/window/GPU)
    // ------------------------------------------------------------------

    use bitty_plugin_host::{
        CapabilityId, Event as HostEvent, EventKind as HostEventKind, EventPayload as HostPayload,
        HostObservation as HostObs, InterceptionDecision as HostDecision, PluginId as HostPid,
        PluginManifest as HostManifest,
    };

    fn host_manifest(id: &str, commands: Vec<&str>, events: Vec<&str>) -> HostManifest {
        use bitty_plugin_host::{
            CapabilityRequests, Compat, LazyTriggers, PluginIdentity, QualifiedName,
        };
        HostManifest {
            identity: PluginIdentity {
                id: HostPid::new(id).unwrap(),
                name: "Test".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            provided_services: Vec::new(),
            capabilities: CapabilityRequests::default(),
            lazy: LazyTriggers {
                commands: commands
                    .into_iter()
                    .map(|c| QualifiedName::new(c).unwrap())
                    .collect(),
                events: events.into_iter().map(|s| s.to_string()).collect(),
                claims: Vec::new(),
            },
            raw_bytes_len: 256,
        }
    }

    #[test]
    fn default_plugin_host_is_drop_oldest_with_bounded_queues() {
        let rt = make_runtime();
        assert_eq!(
            rt.plugin_drop_policy(),
            bitty_plugin_host::DropPolicy::DropOldest
        );
        assert_eq!(
            rt.plugin_pipeline_capacity(),
            DEFAULT_PLUGIN_PIPELINE_CAPACITY
        );
        assert_eq!(rt.plugin_side_capacity(), DEFAULT_PLUGIN_SIDE_CAPACITY);
        assert_eq!(rt.plugin_side_len(), 0);
        assert_eq!(rt.plugin_total_dropped(), 0);
    }

    #[test]
    fn runtime_with_drop_newest_honors_open_point() {
        let cfg = RuntimeConfig::default();
        let rt = Runtime::with_plugin_drop_policy(cfg, bitty_plugin_host::DropPolicy::DropNewest)
            .expect("must build");
        assert_eq!(
            rt.plugin_drop_policy(),
            bitty_plugin_host::DropPolicy::DropNewest
        );
    }

    #[test]
    fn runtime_with_custom_capacities() {
        let cfg = RuntimeConfig::default();
        let rt = Runtime::with_plugin_host_capacity(
            cfg,
            bitty_plugin_host::DropPolicy::DropOldest,
            8,
            16,
        )
        .expect("must build");
        assert_eq!(rt.plugin_pipeline_capacity(), 8);
        assert_eq!(rt.plugin_side_capacity(), 16);
    }

    #[test]
    fn register_plugin_happy_path_and_duplicate_command_rejected() {
        let mut rt = make_runtime();
        let m1 = host_manifest("xuepoo.a", vec!["xuepoo.a:cmd"], vec!["terminal.bell"]);
        rt.register_plugin(m1).expect("first register must succeed");
        assert_eq!(rt.plugin_host().registry().len(), 1);

        // Second plugin claiming same qualified command must be rejected at graph construction.
        let m2 = host_manifest("xuepoo.b", vec!["xuepoo.a:cmd"], vec![]);
        let err = rt
            .register_plugin(m2)
            .expect_err("duplicate command must be rejected");
        assert!(
            err.to_string().contains("already owned"),
            "error must mention duplicate: {err}"
        );
    }

    #[test]
    fn register_plugin_validates_manifest() {
        let mut rt = make_runtime();
        let mut bad = host_manifest("xuepoo.bad", vec![], vec![]);
        bad.raw_bytes_len = bitty_plugin_host::MANIFEST_MAX_BYTES + 1;
        assert!(rt.register_plugin(bad).is_err());
    }

    #[test]
    fn side_queue_bridging_is_bounded_and_never_blocks_hot_path() {
        // Use small side capacity to force drops.
        let cfg = RuntimeConfig {
            cold_queue_capacity: 4,
            ..RuntimeConfig::default()
        };
        let mut rt = Runtime::with_plugin_host_capacity(
            cfg,
            bitty_plugin_host::DropPolicy::DropOldest,
            64,
            2,
        )
        .expect("must build");

        // Feed five title changes; each title yields TitleChanged only (no damage),
        // so cold queue sees 5 events, capacity 4 => 1 dropped; side queue sees
        // 5 observations, capacity 2 => 3 dropped. This proves bounded drops without blocking.
        for name in ["first", "second", "third", "fourth", "fifth"] {
            rt.handle_pty_bytes(format!("\x1b]0;{name}\x07").as_bytes());
        }

        assert_eq!(rt.cold_queue_len(), 4);
        assert_eq!(rt.cold_queue_dropped(), 1);

        assert_eq!(rt.plugin_side_len(), 2);
        assert_eq!(rt.plugin_side_dropped(), 3);
        let obs = rt.drain_plugin_observations();
        assert_eq!(obs.len(), 2);
        assert_eq!(rt.plugin_side_len(), 0);
        // The surviving two are the newest (DropOldest policy).
        assert_eq!(obs[0], HostObs::TitleChanged("fourth".to_string()));
        assert_eq!(obs[1], HostObs::TitleChanged("fifth".to_string()));
    }

    #[test]
    fn handle_pty_bytes_also_pushes_bell_and_mode_to_side_queue() {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"\x07"); // BEL
        let obs = rt.drain_plugin_observations();
        assert!(obs.contains(&HostObs::Bell));

        // `CSI 4 h` enables Insert mode (Mode::Insert) — mapped to ModeChanged and thence to HostObservation.
        rt.handle_pty_bytes(b"\x1b[4h");
        let obs2 = rt.drain_plugin_observations();
        assert!(
            obs2.iter()
                .any(|o| matches!(o, HostObs::ModeChanged { .. })),
            "Insert mode toggle must produce ModeChanged observation, got {obs2:?}"
        );
    }

    #[test]
    fn pipeline_publish_and_drain_via_runtime() {
        let mut rt = make_runtime();
        let m = host_manifest("xuepoo.test", vec![], vec!["terminal.bell"]);
        rt.register_plugin(m).expect("register");
        rt.subscribe_plugin_event(
            &HostPid::new("xuepoo.test").unwrap(),
            HostEventKind::TerminalBell,
        )
        .expect("subscribe");

        rt.publish_plugin_event(HostEvent::new(
            HostEventKind::TerminalBell,
            HostPayload::Empty,
            1,
        ));
        rt.publish_plugin_event(HostEvent::new(
            HostEventKind::TerminalBell,
            HostPayload::Empty,
            2,
        ));

        let batch = rt
            .drain_plugin_events_all(
                &HostPid::new("xuepoo.test").unwrap(),
                &HostEventKind::TerminalBell,
            )
            .expect("drain");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].sequence, 1);
    }

    #[test]
    fn pipeline_bounded_drops_counted_for_doctor() {
        let mut rt = Runtime::with_plugin_host_capacity(
            RuntimeConfig::default(),
            bitty_plugin_host::DropPolicy::DropNewest,
            2,
            64,
        )
        .expect("must build");
        let m = host_manifest("xuepoo.test", vec![], vec!["terminal.bell"]);
        rt.register_plugin(m).unwrap();
        rt.subscribe_plugin_event(
            &HostPid::new("xuepoo.test").unwrap(),
            HostEventKind::TerminalBell,
        )
        .unwrap();

        for i in 0..5 {
            rt.publish_plugin_event(HostEvent::new(
                HostEventKind::TerminalBell,
                HostPayload::Empty,
                i,
            ));
        }
        assert!(rt.plugin_total_dropped() > 0);
        let per = rt.plugin_dropped_per_queue();
        assert!(!per.is_empty());
        let drained = rt
            .drain_plugin_events_all(
                &HostPid::new("xuepoo.test").unwrap(),
                &HostEventKind::TerminalBell,
            )
            .unwrap();
        assert_eq!(drained.len(), 2); // capacity 2 with DropNewest keeps oldest 2.
    }

    #[test]
    fn grant_check_stub_deny_by_default_and_hash_binding() {
        let mut rt = make_runtime();
        let m = host_manifest("xuepoo.test", vec!["xuepoo.test:cmd"], vec![]);
        rt.register_plugin(m).unwrap();
        let pid = HostPid::new("xuepoo.test").unwrap();
        let cap = CapabilityId::parse("terminal.semantic-read").unwrap();
        let hash = "abc123";
        // No grant yet -> denied.
        assert!(!rt.is_capability_granted(&pid, hash, &cap));
        assert!(rt.check_command_grant(&pid, hash, &cap).is_err());

        // Insert grant and then succeed.
        let mut granted = std::collections::BTreeSet::new();
        granted.insert(cap.clone());
        rt.insert_grant(GrantRecord::granted(pid.clone(), hash, granted, 1));
        assert!(rt.is_capability_granted(&pid, hash, &cap));
        assert!(rt.check_command_grant(&pid, hash, &cap).is_ok());
        // Wrong hash denies.
        assert!(!rt.is_capability_granted(&pid, "other", &cap));
        assert!(rt.check_command_grant(&pid, "other", &cap).is_err());
    }

    #[test]
    fn dispatch_command_checks_ownership_and_grant() {
        let mut rt = make_runtime();
        let m = host_manifest("xuepoo.test", vec!["xuepoo.test:run"], vec![]);
        rt.register_plugin(m).unwrap();
        let pid = HostPid::new("xuepoo.test").unwrap();
        let cap = CapabilityId::parse("ui.rich").unwrap();
        let hash = "h";
        let qn = bitty_plugin_host::QualifiedName::new("xuepoo.test:run").unwrap();

        // Without grant -> dispatch denied.
        assert!(rt.dispatch_command(&pid, &qn, hash, &cap).is_err());

        let mut granted = std::collections::BTreeSet::new();
        granted.insert(cap.clone());
        rt.insert_grant(GrantRecord::granted(pid.clone(), hash, granted, 1));
        assert!(rt.dispatch_command(&pid, &qn, hash, &cap).is_ok());

        // Wrong qualified name -> not owned.
        let other = bitty_plugin_host::QualifiedName::new("xuepoo.test:other").unwrap();
        assert!(rt.dispatch_command(&pid, &other, hash, &cap).is_err());
    }

    #[test]
    fn interception_veto_wins_and_fail_open() {
        assert!(!Runtime::intercept_command_dispatch(
            &[HostDecision::Approve, HostDecision::Veto],
            false
        ));
        assert!(Runtime::intercept_command_dispatch(
            &[HostDecision::Approve, HostDecision::Veto],
            true
        ));
        assert!(Runtime::intercept_paste(&[HostDecision::Approve], false));
        assert!(!Runtime::intercept_open_url(&[HostDecision::Veto], false));
        assert!(Runtime::intercept_terminal_spawn(&[], false));
    }

    #[test]
    fn safe_mode_rejects_third_party_via_host() {
        let mut rt = make_runtime();
        rt.set_plugin_safe_mode(true);
        assert!(rt.plugin_safe_mode());
        let m = host_manifest("xuepoo.third", vec![], vec![]);
        assert!(rt.register_plugin(m).is_err());
        let builtin = host_manifest("bitty.core", vec![], vec![]);
        assert!(rt.register_plugin(builtin).is_ok());
    }

    #[test]
    fn no_lua_vm_window_gpu_coupling_in_runtime_api() {
        // Compile-time proof: Runtime constructs headlessly without window/GPU/Lua.
        let rt = make_runtime();
        assert!(rt.is_headless());
        assert!(rt.plugin_host().side_queue().is_empty());
        assert_eq!(rt.plugin_host().pipeline().queue_count(), 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_plugin_wiring_compiles_and_is_headless() {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"hi");
        let _ = rt.tick();
        assert!(rt.is_headless());
        assert!(rt.plugin_side_len() <= rt.plugin_side_capacity());
    }
}
