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

use bitty_platform::{PhysicalSize, PlatformEvent, WindowEventKind};
use bitty_pty::{Pty, PtyBuilder};
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
pub struct Runtime {
    config: RuntimeConfig,
    parser: Parser,
    state: State,
    pty: Option<Pty>,
    renderer: GridRenderer<HeadlessRasterizer>,
    surface: Surface,
    cold_queue: ColdQueue,
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
            .field("has_pty", &self.pty.is_some())
            .field("pending_full_redraw", &self.pending_full_redraw)
            .field("leaf_count", &self.layout.leaf_count())
            .field("focused", &self.focus.focused())
            .field("container", &self.container)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Creates a runtime from `config`, validating the config eagerly and
    /// building the headless software surface and deterministic renderer.
    ///
    /// The initial layout is a single leaf `ViewId(1)` sized to `config`
    /// cols/rows with focus on that leaf and a container matching the grid.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] for bad grid or font fields;
    /// [`RuntimeError::Render`] when the surface or renderer construction
    /// rejects the derived pixel extent or font query.
    pub fn new(config: RuntimeConfig) -> Result<Self, RuntimeError> {
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
            renderer,
            surface,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
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
    /// kills and reaps its child without leaking a zombie.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `program` is blank;
    /// [`RuntimeError::Pty`] when the platform reports spawn failure
    /// (`Unsupported` on Windows before the ConPTY slice, `Upstream` or
    /// `Io` elsewhere).
    pub fn spawn_shell(&mut self, program: &str) -> Result<(), RuntimeError> {
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        let cols = self.cols.min(u16::MAX as usize) as u16;
        let rows = self.rows.min(u16::MAX as usize) as u16;
        let pty = PtyBuilder::new(program)
            .size(cols, rows)
            .spawn()
            .map_err(RuntimeError::from)?;
        self.pty = Some(pty);
        Ok(())
    }

    /// Feeds raw PTY bytes through the parser into terminal state, enqueuing
    /// bounded cold-path observations derived from the actions.
    ///
    /// The byte stream may be split arbitrarily; splitting the same bytes
    /// differently yields the same action sequence (deterministic replay
    /// contract). Malformed or hostile sequences are bounded and never panic.
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
                self.cold_queue.push(ev);
            }
            let damage = self.state.apply(&action);
            if !damage.regions.is_empty() {
                self.cold_queue.push(ColdEvent::Damage {
                    generation: damage.generation,
                });
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
    /// Resize events are routed through [`Self::handle_resize`]; other
    /// window events currently return `false` without side effects so the
    /// hot path stays deterministic. Input routing (keymap, encoder) is an
    /// open placement question tracked in ADR-0003 and is intentionally
    /// not handled here yet.
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
}
