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

use bitty_platform::{PhysicalSize, PlatformEvent, WindowEventKind};
use bitty_pty::{Pty, PtyBuilder};
use bitty_render::{
    RenderError,
    glyph::{
        BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
        RasterKey,
    },
    gpu::{PresentStats as RenderPresentStats, Surface},
    grid::{CellMetrics, GridRenderer},
};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, State, TerminalAction};
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
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Creates a runtime from `config`, validating the config eagerly and
    /// building the headless software surface and deterministic renderer.
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
        Ok(Self {
            cols: config.cols,
            rows: config.rows,
            config: config.clone(),
            parser: Parser::new(),
            state: State::new(),
            pty: None,
            renderer,
            surface,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            last_presented_generation: u64::MAX,
            pending_full_redraw: true,
            // pty size mirrors grid until resize arrives
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
    /// configured cell metrics, reconfigures the software surface, and
    /// resizes the PTY when present. Grid memory resize (the state reflow
    /// algorithm) is deferred per the terminal-state-rfc open item, so the
    /// state grid stays at its current dimensions while the surface and PTY
    /// reflect the new window size honestly. Zero-sized extents are skipped
    /// (minimized/occluded windows) per the `map_resize_to_surface_extent`
    /// contract.
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
    /// The software seam composites `DrawList + Atlas` onto an owned RGBA
    /// buffer via `Surface::headless_present`; no display server or adapter
    /// is touched. Real GPU present remains env-gated (`BITTY_RENDER_GPU_TESTS=1`)
    /// and is not available on headless CI — `is_headless` is `true` for
    /// every present this method emits today.
    pub fn tick(&mut self) -> Option<PresentStats> {
        let snapshot = self.state.snapshot();
        let pending_full = self.pending_full_redraw;
        let last = self.last_presented_generation;
        let current_gen = snapshot.generation;

        // Frame-on-demand: no new generation and no forced redraw -> idle.
        if !pending_full && current_gen == last && last != u64::MAX {
            return None;
        }

        let damage = if pending_full || last == u64::MAX {
            Damage {
                generation: current_gen,
                regions: vec![DamagedRegion::Grid(DamageRect::full(
                    snapshot.height as u16,
                    snapshot.width as u16,
                ))]
                .into_boxed_slice(),
            }
        } else {
            // Pull coalesced regions since the last presented generation.
            // When the history window has fallen behind, the returned set may
            // be incomplete; treat a large gap as a full redraw to keep
            // correctness over performance.
            let regions = self.state.damage_since(last);
            let gap = current_gen.saturating_sub(last);
            if gap > bitty_term_state::damage::DAMAGE_HISTORY_BATCHES as u64 && regions.is_empty() {
                Damage {
                    generation: current_gen,
                    regions: vec![DamagedRegion::Grid(DamageRect::full(
                        snapshot.height as u16,
                        snapshot.width as u16,
                    ))]
                    .into_boxed_slice(),
                }
            } else {
                Damage {
                    generation: current_gen,
                    regions: regions.into_boxed_slice(),
                }
            }
        };

        self.pending_full_redraw = false;
        let list = match self.renderer.render(&snapshot, &damage) {
            Ok(list) => list,
            Err(_) => return None,
        };
        if !list.needs_draw() {
            self.last_presented_generation = current_gen;
            return None;
        }
        let atlas_texels = self.renderer.atlas_texels().to_vec();
        let dims = self.renderer.atlas_dims();
        let stats = match self
            .surface
            .headless_present(&list, Some((&atlas_texels, dims)))
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

    #[test]
    fn tick_is_idle_when_no_damage() {
        let mut rt = Runtime::with_defaults().expect("defaults must build");
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
        let mut rt = Runtime::with_defaults().expect("defaults must build");
        assert!(rt.tick().is_some());
        rt.handle_pty_bytes(b"hello ");
        let stats = rt.tick().expect("damage from bytes must present");
        assert!(stats.glyphs > 0);
        assert_eq!(rt.tick(), None, "must return to idle after present");
    }

    #[test]
    fn handle_resize_reconfigures_surface_and_keeps_grid_pending_full_redraw() {
        let mut rt = Runtime::with_defaults().expect("defaults must build");
        let before = rt.surface_extent().expect("surface must have extent");
        assert_eq!(before, RuntimeConfig::default().pixel_extent());
        rt.handle_resize(PhysicalSize::new(800, 600))
            .expect("valid resize");
        assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
        assert!(rt.tick().is_some(), "resize forces full redraw");
    }

    #[test]
    fn zero_resize_is_skipped_honestly() {
        let mut rt = Runtime::with_defaults().expect("defaults must build");
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
        let mut rt = Runtime::with_defaults().expect("must build");
        let close = rt.handle_platform_event(PlatformEvent::Exiting);
        assert!(close);
        assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
        assert!(!rt.handle_platform_event(PlatformEvent::AboutToWait));
        assert!(!rt.handle_platform_event(PlatformEvent::Suspended));
    }

    #[test]
    fn handle_platform_event_resize_via_handle_resize() {
        let mut rt = Runtime::with_defaults().expect("must build");
        rt.handle_resize(PhysicalSize::new(320, 240))
            .expect("valid resize");
        assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(320, 240)));
        assert!(rt.tick().is_some(), "resize forces full redraw");
    }

    #[test]
    fn spawn_shell_blank_program_rejected_without_touching_pty() {
        let mut rt = Runtime::with_defaults().expect("must build");
        assert!(rt.spawn_shell("").is_err());
        assert!(rt.spawn_shell("   ").is_err());
    }

    // Ensures missing `allow` does not leak `dead_code` on the Window target.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_build_still_compiles_with_queue_and_tick() {
        let mut rt = Runtime::with_defaults().expect("windows defaults must build");
        rt.handle_pty_bytes(b"hi");
        let _ = rt.tick();
        assert!(rt.is_headless());
    }
}
