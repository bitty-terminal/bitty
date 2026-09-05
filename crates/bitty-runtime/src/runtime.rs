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
//! Per-pane shells (CTX-0176): every split leaf may own a private
//! [`PaneSession`](Runtime::spawn_shell_for_view) — its own parser, grid
//! state, and PTY triple — so panes stop mirroring the single primary
//! session. Leaves without a session keep rendering the shared primary
//! state (the unchanged single-pane path). Input routes to the focused
//! leaf's writer only (see [`Runtime::push_input_bytes`]); [`Runtime::tick`]
//! renders each leaf from its own grid; [`Runtime::poll_pty`] drains every
//! session. [`Runtime::close_pane_session`] tears a leaf's child down.
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

use std::collections::BTreeMap;

use bitty_platform::{
    Clipboard, CursorPosition, KeyEvent, MouseButton, PhysicalSize, PlatformEvent, PressState,
    ScaleFactor, ScrollDelta, WindowEventKind,
};
use bitty_pty::{Pty, PtyBuilder, PtyReader, PtyWriter};
use bitty_render::{
    CrossFontRasterizer, RenderError,
    frame::{FrameMode, FramePlan},
    glyph::{
        BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
        RasterKey,
    },
    gpu::{GpuContext, PresentStats as RenderPresentStats, Surface},
    grid::{CellMetrics, DrawList, GridRenderer},
    grid_from_surface_extent, sanitize_dpi_scale,
};
use bitty_term_state::search::{SearchMatch, SearchOptions};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction};
use bitty_ui::{
    CellPos, Focus, FocusDirection, LayoutNode, PersistentSelection, Rect as UiRect,
    SearchHighlight, Selection, SelectionKind, View, ViewId, search::SearchState,
};
use bitty_vt::{ClipboardOp, Parser, SequenceKind};

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

/// Owned rasterizer that can be either deterministic headless or real crossfont.
///
/// Headless is the CI baseline (no font file, bit-identical everywhere).
/// Crossfont is the vertical-slice real path (platform font stack).
/// Both implement `GlyphRasterizer` so `GridRenderer` stays generic.
#[derive(Debug)]
enum AnyRasterizer {
    Headless(HeadlessRasterizer),
    CrossFont(Box<CrossFontRasterizer>),
}

impl AnyRasterizer {
    fn try_crossfont() -> Self {
        match CrossFontRasterizer::new() {
            Ok(cf) => Self::CrossFont(Box::new(cf)),
            Err(_) => Self::Headless(HeadlessRasterizer::new()),
        }
    }
    fn is_crossfont(&self) -> bool {
        matches!(self, Self::CrossFont(_))
    }
}

impl GlyphRasterizer for AnyRasterizer {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        match self {
            Self::Headless(r) => r.load_font(query),
            Self::CrossFont(r) => r.load_font(query),
        }
    }
    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        match self {
            Self::Headless(r) => r.rasterize(key),
            Self::CrossFont(r) => r.rasterize(key),
        }
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

fn clamp_cell_pos(snapshot: &Snapshot, pos: CellPos) -> CellPos {
    let max_row = snapshot.height.saturating_sub(1) as u16;
    let max_col = snapshot.width.saturating_sub(1) as u16;
    CellPos::new(pos.row.min(max_row), pos.col.min(max_col))
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

/// Bounded human-readable label for a [`KeyEvent`] (CTX-0159 input ring).
///
/// Prefers the layout-dependent `text` when present (so `wtype` probes show
/// the typed character), else the logical key name. Truncated to 32
/// characters here; the ring applies the final [`crate::inspect`] bound.
fn key_inspect_label(event: &KeyEvent) -> String {
    if let Some(text) = event.text.as_deref() {
        let first = text.chars().next().unwrap_or('?');
        if !text.is_empty() && text.chars().count() == 1 && !first.is_control() {
            return format!("key:{text}");
        }
    }
    match &event.logical_key {
        bitty_platform::LogicalKey::Character(s) => {
            let short: String = s.chars().take(8).collect();
            format!("key:{short}")
        }
        bitty_platform::LogicalKey::Named(named) => format!("key:{named:?}"),
        _ => "key:Unidentified".to_string(),
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

/// Cross-thread PTY readability callback.
///
/// Invoked exactly once per readability signal (per forwarded chunk plus once
/// on EOF) from the forwarder thread — never on a timer, never when quiet.
/// Production wires this to [`bitty_platform::EventWaker::wake_pty`];
/// headless tests wire it to a counter/channel. Only `Send` is required:
/// the forwarder thread owns its clone and is the sole caller.
pub type PtyWaker = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;

/// Forwarding channel capacity for the wakeup pump (chunks).
///
/// Matches [`bitty_pty::CHANNEL_CAPACITY_CHUNKS`] so each stage stays within
/// the documented bound. Worst-case total buffered when the wakeup pump is
/// active is 2 x 128 KiB (original pump channel plus forwarding channel),
/// still bounded, fail-closed, and backpressured end to end.
pub const PTY_FORWARD_CAPACITY_CHUNKS: usize = bitty_pty::CHANNEL_CAPACITY_CHUNKS;

/// Blocking forwarder: sole consumer of `reader`, pushing into `tx` and
/// waking once per chunk plus once on EOF.
///
/// - Quiet child: parked in `recv`, zero wakeups, zero CPU.
/// - Backpressure: `send` blocks when `tx` is full, which fills the original
///   pump channel, which fills the kernel PTY buffer, which blocks the child.
/// - Fail-closed: a dropped consumer breaks `send` and ends the thread with
///   no loss beyond already-queued chunks and no unbounded growth.
fn pty_forward_loop(reader: PtyReader, tx: std::sync::mpsc::SyncSender<Vec<u8>>, waker: PtyWaker) {
    loop {
        match reader.recv() {
            Some(chunk) => {
                debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                if tx.send(chunk).is_err() {
                    break;
                }
                (waker)();
            }
            None => {
                // EOF: wake once so the consumer drains final chunks promptly
                // instead of waiting for an incidental wakeup.
                (waker)();
                break;
            }
        }
    }
    // Reap the pump thread; outcome is informational (EOF vs I/O error).
    let _ = reader.join();
}

/// One split pane's private shell session (CTX-0176).
///
/// Each leaf created by a split owns at most one of these: its own VT
/// parser, terminal grid state, query-overlap tail, and PTY triple. The
/// owned [`Pty`] keeps the child alive — dropping the session kills and
/// reaps it without leaking a zombie. The reader stays direct (drained by
/// [`Runtime::poll_pty`]); no wakeup-forwarder promotion applies to panes.
struct PaneSession {
    parser: Parser,
    state: State,
    query_overlap: Vec<u8>,
    pty: Pty,
    reader: PtyReader,
    writer: PtyWriter,
}

/// Full compact banner visible duration (CTX-0192).
///
/// A gated paste shows the compact one-line summary for this long, then
/// collapses to [`PASTE_BANNER_FLASH_TEXT`] while the paste still pends.
/// The flash keeps the never-silent signal without occluding the grid.
pub const PASTE_BANNER_FULL_DURATION: std::time::Duration = std::time::Duration::from_secs(4);

/// Minimal status flash while a paste pends after the full banner expires
/// (CTX-0192). Bounded, single-line, always `Some` while pending.
pub const PASTE_BANNER_FLASH_TEXT: &str = "Paste… repeat=confirm Esc=cancel";

pub struct Runtime {
    config: RuntimeConfig,
    parser: Parser,
    state: State,
    pty: Option<Pty>,
    pty_reader: Option<PtyReader>,
    /// Wakeup-pump forwarding receiver (active after [`Runtime::set_pty_waker`]
    /// promotes the direct reader). Bounded [`PTY_FORWARD_CAPACITY_CHUNKS`].
    pty_forward_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    /// Forwarder thread handle (detached on respawn; exits on EOF/disconnect).
    pty_forward_handle: Option<std::thread::JoinHandle<()>>,
    /// Readability callback moved (cloned) into the forwarder on promotion.
    pty_waker: Option<PtyWaker>,
    pty_writer: Option<PtyWriter>,
    /// Private shell sessions keyed by split-leaf id (CTX-0176). Empty in
    /// single-pane use, where the primary PTY/state path is unchanged.
    pane_sessions: BTreeMap<ViewId, PaneSession>,
    pending_input: Vec<u8>,
    pending_input_dropped: u64,
    renderer: GridRenderer<AnyRasterizer>,
    surface: Surface,
    gpu: Option<GpuContext>,
    cold_queue: ColdQueue,
    plugin_host: PluginHost,
    last_presented_generation: u64,
    pending_full_redraw: bool,
    cols: usize,
    rows: usize,
    layout: LayoutNode,
    focus: Focus,
    container: UiRect,
    clipboard: Clipboard,
    selection: Option<Selection>,
    selection_dragging: bool,
    /// Last clipboard failure observed on the mouse-paste path (CTX-0158).
    ///
    /// Ghostty copies a committed left-drag selection to both the standard
    /// clipboard and the selection/primary clipboard; middle-click then
    /// pastes from the primary selection. Both selections live in
    /// `bitty-platform::Clipboard`, which is Wayland-first on Linux
    /// (`wayland-data-control` backend when `WAYLAND_DISPLAY` is set, X11
    /// fallback, fail-soft headless buffers otherwise — CTX-0160): there is
    /// no runtime-side primary buffer, so cross-app copy/paste works in both
    /// directions without a second source of truth.
    ///
    /// Reads/writes on the mouse path stay fail-soft (no paste, no panic,
    /// no block), but failures are recorded here instead of swallowed, so
    /// the embedder can surface them. A subsequent successful clipboard
    /// operation clears the slot. Bounded: at most one retained error.
    last_clipboard_error: Option<bitty_platform::PlatformError>,
    last_cursor: Option<CursorPosition>,
    search_state: SearchState,
    pending_paste: Option<crate::paste::PendingPaste>,
    /// Wall time when the current pending paste was gated (CTX-0192).
    ///
    /// Drives the transient banner: full summary for
    /// [`PASTE_BANNER_FULL_DURATION`], then a minimal flash while pending.
    /// `None` when no paste pends. Set with `Instant::now()` in
    /// `request_paste`; cleared on confirm/cancel.
    pending_paste_since: Option<std::time::Instant>,
    /// Whether the banner has collapsed to the flash phase (CTX-0192).
    ///
    /// Tracks the last painted phase so `tick` can force exactly one
    /// repaint on the full→flash transition, then idle with the flash
    /// retained on screen (never-silent while pending).
    paste_banner_collapsed: bool,
    osc_clipboard_read_allowed: bool,
    osc_clipboard_write_allowed: bool,
    pending_activation_gesture: Option<ActivationGesture>,
    next_activation_gesture: u64,
    // Input/Pointer RFC (CTX-0107) state for single-window slice
    kitty_flags: u32,
    shift_pressed: bool,
    control_pressed: bool,
    alt_pressed: bool,
    // Focus/mouse capture tracking per lifecycle RFC
    focused: bool,
    mouse_capture_enabled: bool,
    // IME composition overlay (presentation only, not Terminal Truth)
    ime_preedit: Option<String>,
    ime_cursor: usize,
    // Wheel accumulator for pixel scroll (candidate: 4*cell bound)
    wheel_accum_y: f32,
    wheel_accum_x: f32,
    // CTX-0185: fractional line-notch accumulator. `Lines` deltas are f32
    // (high-resolution wheels emit fractions of a notch); truncating each
    // event to `isize` dropped sub-notch motion entirely, which read as lag.
    // Scaled notches accumulate here and emit whole lines; clamped to one
    // frame's cap so a spinning wheel cannot bank unbounded drift.
    wheel_line_accum_y: f32,
    wheel_line_accum_x: f32,
    // DPI scale
    scale_factor: ScaleFactor,
    is_crossfont: bool,
    /// Tail of previously seen PTY bytes retained so a terminal query split
    /// over two PTY reads is still recognized (CTX-0146). Bounded by
    /// [`crate::queries::QUERY_OVERLAP_MAX`]; raw query scans never retain
    /// more.
    query_overlap: Vec<u8>,
    /// Bounded ring of the last input events for screenshots-free debugging
    /// (CTX-0159, Issue #258). Published read-only to the `BITTY_SOCKET`
    /// introspection store; never affects terminal truth or PTY bytes.
    inspect_ring: crate::inspect::InputRing,
}

/// Opaque, runtime-issued proof of a platform input gesture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationGesture(u64);

/// Runtime-issued authorization for a non-local URL activation.
#[derive(Debug, PartialEq, Eq)]
pub struct UrlActivation {
    uri: String,
}

/// Runtime-issued authorization for a local-file URL activation.
#[derive(Debug, PartialEq, Eq)]
pub struct FileUrlActivation {
    uri: String,
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
            .field(
                "has_pty_reader",
                &(self.pty_reader.is_some() || self.pty_forward_rx.is_some()),
            )
            .field("has_pty_writer", &self.pty_writer.is_some())
            .field("pane_sessions", &self.pane_sessions.len())
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
            .field("has_selection", &self.selection.is_some())
            .field("selection_dragging", &self.selection_dragging)
            .field("clipboard_headless", &self.clipboard.is_headless())
            .field("search_active", &self.search_state.is_active())
            .field("search_matches", &self.search_state.match_count())
            .field("search_current", &self.search_state.current_index())
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
        // Vertical slice: prefer crossfont when available, fallback to headless
        // for CI determinism. Both are bounded and headless-testable. On
        // Windows the monospace family may be absent, so a FontNotFound from
        // GridRenderer re-tries deterministically with HeadlessRasterizer
        // instead of failing with_defaults on headless CI.
        let (renderer, is_crossfont) = {
            let raster = AnyRasterizer::try_crossfont();
            let is_cf = raster.is_crossfont();
            match GridRenderer::new(raster, &query, cell) {
                Ok(r) => (r, is_cf),
                Err(err) if is_cf && matches!(&err, RenderError::FontNotFound(_)) => {
                    let fallback = AnyRasterizer::Headless(HeadlessRasterizer::new());
                    let r =
                        GridRenderer::new(fallback, &query, cell).map_err(RuntimeError::from)?;
                    (r, false)
                }
                Err(err) => return Err(RuntimeError::from(err)),
            }
        };
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
            pty_forward_rx: None,
            pty_forward_handle: None,
            pty_waker: None,
            pty_writer: None,
            pane_sessions: BTreeMap::new(),
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            gpu: None,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            pending_full_redraw: true,
            layout,
            focus,
            container,
            clipboard: Clipboard::new(),
            selection: None,
            selection_dragging: false,
            last_clipboard_error: None,
            last_cursor: None,
            search_state: SearchState::new(),
            pending_paste: None,
            pending_paste_since: None,
            paste_banner_collapsed: false,
            osc_clipboard_read_allowed: false,
            osc_clipboard_write_allowed: false,
            pending_activation_gesture: None,
            next_activation_gesture: 1,
            kitty_flags: 0,
            shift_pressed: false,
            control_pressed: false,
            alt_pressed: false,
            focused: true,
            mouse_capture_enabled: false,
            ime_preedit: None,
            ime_cursor: 0,
            wheel_accum_y: 0.0,
            wheel_accum_x: 0.0,
            wheel_line_accum_y: 0.0,
            wheel_line_accum_x: 0.0,
            scale_factor: ScaleFactor::ONE,
            is_crossfont,
            query_overlap: Vec::new(),
            inspect_ring: crate::inspect::InputRing::new(),
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
        let (renderer, is_crossfont) = {
            let raster = AnyRasterizer::try_crossfont();
            let is_cf = raster.is_crossfont();
            match GridRenderer::new(raster, &query, cell) {
                Ok(r) => (r, is_cf),
                Err(err) if is_cf && matches!(&err, RenderError::FontNotFound(_)) => {
                    let fallback = AnyRasterizer::Headless(HeadlessRasterizer::new());
                    let r =
                        GridRenderer::new(fallback, &query, cell).map_err(RuntimeError::from)?;
                    (r, false)
                }
                Err(err) => return Err(RuntimeError::from(err)),
            }
        };
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
            pty_forward_rx: None,
            pty_forward_handle: None,
            pty_waker: None,
            pty_writer: None,
            pane_sessions: BTreeMap::new(),
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            gpu: None,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            pending_full_redraw: true,
            layout,
            focus,
            container,
            clipboard: Clipboard::new(),
            selection: None,
            selection_dragging: false,
            last_clipboard_error: None,
            last_cursor: None,
            search_state: SearchState::new(),
            pending_paste: None,
            pending_paste_since: None,
            paste_banner_collapsed: false,
            osc_clipboard_read_allowed: false,
            osc_clipboard_write_allowed: false,
            pending_activation_gesture: None,
            next_activation_gesture: 1,
            kitty_flags: 0,
            shift_pressed: false,
            control_pressed: false,
            alt_pressed: false,
            focused: true,
            mouse_capture_enabled: false,
            ime_preedit: None,
            ime_cursor: 0,
            wheel_accum_y: 0.0,
            wheel_accum_x: 0.0,
            wheel_line_accum_y: 0.0,
            wheel_line_accum_x: 0.0,
            scale_factor: ScaleFactor::ONE,
            is_crossfont,
            query_overlap: Vec::new(),
            inspect_ring: crate::inspect::InputRing::new(),
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

    /// Attempts to attach a real GPU surface (vertical slice: one window/one terminal).
    ///
    /// The caller has created `gpu` via `GpuContext::initialize().await` and a
    /// `Surface` via `gpu.create_surface(&target)`. This method stores the
    /// GPU context so `tick` can present via `Surface::present_draw_list` with
    /// the real swapchain. When no GPU is available the headless seam remains.
    pub fn attach_gpu(&mut self, gpu: GpuContext, surface: Surface) {
        // Keep renderer as is (AnyRasterizer may be crossfont already); surface
        // and gpu are swapped wholesale. Headless fallback remains if gpu later
        // fails present (caller may detach).
        self.surface = surface;
        self.gpu = Some(gpu);
        self.pending_full_redraw = true;
    }

    /// Detaches GPU, falling back to headless surface at current extent.
    pub fn detach_gpu(&mut self) {
        if let Some(extent) = self.surface.extent() {
            if let Ok(s) = Surface::headless(extent) {
                self.surface = s;
            }
        }
        self.gpu = None;
        self.pending_full_redraw = true;
    }

    /// Whether a real GPU surface is attached (not headless).
    #[must_use]
    pub fn has_gpu(&self) -> bool {
        self.gpu.is_some() && !self.surface.is_headless()
    }

    /// Current Kitty keyboard flags (7727 bitmask). 0 = legacy.
    #[must_use]
    pub fn kitty_flags(&self) -> u32 {
        self.kitty_flags
    }

    /// Whether Shift is currently latched (CTX-0159 read-only accessor).
    #[must_use]
    pub fn shift_pressed(&self) -> bool {
        self.shift_pressed
    }

    /// Whether Control is currently latched (CTX-0159 read-only accessor).
    #[must_use]
    pub fn control_pressed(&self) -> bool {
        self.control_pressed
    }

    /// Whether Alt is currently latched (CTX-0159 read-only accessor).
    #[must_use]
    pub fn alt_pressed(&self) -> bool {
        self.alt_pressed
    }

    /// Whether the window currently holds keyboard focus (CTX-0159).
    #[must_use]
    pub fn is_window_focused(&self) -> bool {
        self.focused
    }

    /// Whether mouse-event capture is active (CTX-0159 read-only accessor).
    #[must_use]
    pub fn mouse_capture_active(&self) -> bool {
        self.mouse_capture_enabled
    }

    /// Snapshot of the bounded input ring, oldest first (CTX-0159).
    #[must_use]
    pub fn inspect_input_snapshot(&self, limit: usize) -> Vec<crate::inspect::InputEvent> {
        self.inspect_ring.snapshot(limit)
    }

    /// Number of retained input-ring events (CTX-0159).
    #[must_use]
    pub fn inspect_input_len(&self) -> usize {
        self.inspect_ring.len()
    }

    /// Publish bounded read-only introspection snapshots to the `BITTY_SOCKET`
    /// live store (CTX-0159, Issue #258).
    ///
    /// Copies grid text, the input ring, modifier latches, and focus/window
    /// state into `bitty-ipc` globals (`&self` only; never mutates terminal
    /// truth, never writes to the PTY, never blocks). Called on input and on
    /// tick so socket probes observe typed text without screenshots.
    pub fn publish_inspect_snapshot(&self) {
        let grid = crate::inspect::grid_text_from_state(
            &self.state,
            crate::inspect::INSPECT_MAX_ROWS,
            crate::inspect::INSPECT_MAX_COLS,
        );
        crate::inspect::publish_grid(&grid);
        crate::inspect::publish_input_ring(&self.inspect_ring.snapshot_all());
        crate::inspect::publish_modifiers(&crate::inspect::ModifierSnapshot {
            shift: self.shift_pressed,
            control: self.control_pressed,
            alt: self.alt_pressed,
            kitty_flags: self.kitty_flags,
        });
        crate::inspect::publish_focus(&crate::inspect::FocusSnapshot {
            focused: self.focused,
            focused_view: self.focus.focused().map(|v| v.0),
            mouse_capture: self.mouse_capture_enabled,
            alt_screen: self.state.alt_screen_active(),
            bracketed_paste: self.state.modes().bracketed_paste,
            focus_events: self.state.modes().focus_events,
        });
    }

    /// Whether crossfont rasterizer is active.
    #[must_use]
    pub fn is_crossfont(&self) -> bool {
        self.is_crossfont
    }

    /// Current IME preedit overlay, if any (presentation only).
    #[must_use]
    pub fn ime_preedit(&self) -> Option<&str> {
        self.ime_preedit.as_deref()
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
    /// release/synthetic/modifier-only/unmapped inputs. This entry point
    /// assumes no modifiers are held; the live input path
    /// ([`Self::handle_key_event`]) applies the tracked modifier snapshot via
    /// [`encode_key_with_kitty`](Self::encode_key_with_kitty) instead, so
    /// `Ctrl+letter` synthesizes C0 bytes on Wayland where winit reports
    /// `text=None` (CTX-0154).
    #[must_use]
    pub fn encode_key_event(event: &KeyEvent) -> Option<Vec<u8>> {
        bitty_platform::encode_key_event(event)
    }

    /// Snapshots the tracked modifier flags for the legacy encoder.
    ///
    /// winit delivers modifiers (`ModifiersChanged`, modifier key presses)
    /// separately from the key press itself, and on Wayland the press carries
    /// `text=None` (or the bare letter) for `Ctrl+letter`. The legacy encoder
    /// therefore cannot rely on `text` and synthesizes C0/ESC bytes from this
    /// snapshot instead (CTX-0154).
    fn modifier_snapshot(&self) -> bitty_platform::ModifiersState {
        bitty_platform::ModifiersState {
            shift: self.shift_pressed,
            control: self.control_pressed,
            alt: self.alt_pressed,
            super_pressed: false,
        }
    }

    /// Encodes with Kitty protocol when `kitty_flags != 0` (opt-in 7727).
    /// Bounded to 64 bytes per spec; progressive flags are honored with fallback to legacy when flag not set.
    fn encode_key_with_kitty(&self, event: &KeyEvent) -> Option<Vec<u8>> {
        if self.kitty_flags == 0 {
            return bitty_platform::keyboard::encode_key_event_with_modifiers(
                event,
                &self.modifier_snapshot(),
            );
        }
        // Kitty active: produce CSI u for disambiguated keys, else legacy with fallback.
        // For vertical slice, handle Tab vs Ctrl-I disambiguation and Enter vs Ctrl-M.
        // When event is Tab with Ctrl modifier (text is \t but logical is Tab), encode Kitty distinct.
        // Simplistic: if logical is Named(Tab) and control_pressed, encode Kitty 9;1:1 etc.
        // General: encode any character key as CSI <codepoint> ; mods u
        // Bounded: single key ≤64 bytes, checked.
        let mods: u8 = (if self.shift_pressed { 1 } else { 0 })
            | (if self.alt_pressed { 2 } else { 0 })
            | (if self.control_pressed { 4 } else { 0 });
        // For named keys with CSI u equivalents, use codepoint of logical char if available
        let codepoint_opt = match &event.logical_key {
            bitty_platform::LogicalKey::Character(s) => s.chars().next().map(|c| c as u32),
            bitty_platform::LogicalKey::Named(named) => match named {
                bitty_platform::NamedKey::Enter => Some(13),
                bitty_platform::NamedKey::Tab => Some(9),
                bitty_platform::NamedKey::Backspace => Some(127),
                bitty_platform::NamedKey::Escape => Some(27),
                _ => None,
            },
            _ => None,
        };
        if let Some(cp) = codepoint_opt {
            // Kitty CSI u: ESC [ <codepoint> ; <mods+1> u  (mods+1 per Kitty spec where 1 = no mods)
            // Event type 1 = press, 2 = repeat, 3 = release (only when requested flag bit 1 set)
            let event_type = if event.repeat { 2 } else { 1 };
            // Only emit Kitty when flag for disambiguation wants it; for slice, always when Kitty active and mods non-zero or named.
            let is_named = matches!(&event.logical_key, bitty_platform::LogicalKey::Named(_));
            if mods != 0 || is_named {
                let seq = format!("\x1b[{cp};{}:{}u", mods + 1, event_type);
                if seq.len() <= 64 {
                    return Some(seq.into_bytes());
                }
            }
        }
        // Fallback to legacy for keys not disambiguated, still honoring the
        // tracked modifier snapshot (CTX-0154: Ctrl+letter synthesis).
        bitty_platform::keyboard::encode_key_event_with_modifiers(event, &self.modifier_snapshot())
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
        let is_modifier = matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(
                bitty_platform::NamedKey::Shift
                    | bitty_platform::NamedKey::Control
                    | bitty_platform::NamedKey::Alt
                    | bitty_platform::NamedKey::AltGraph
                    | bitty_platform::NamedKey::Super
                    | bitty_platform::NamedKey::Meta
            )
        );
        self.track_modifiers_from_key(&event);
        // CTX-0166: any real non-modifier key press clears the selection
        // highlight (left-click/Esc/typing dismiss). Clearing uses
        // `clear_selection` so `pending_full_redraw` forces the next tick to
        // present without the highlight — never a frame late. Modifier-only
        // and synthetic events never clear; releases never clear.
        if event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
            && self.selection.is_some()
        {
            self.clear_selection();
        }
        // CTX-0186: Esc while a paste is pending cancels the confirmation
        // dialog. The Esc is consumed (never reaches the PTY) so a dismissal
        // cannot also drive shell/vim state.
        if self.cancel_pending_on_escape(&event) {
            return None;
        }
        // CTX-0159: retain a bounded input trace for screenshots-free probes.
        let pressed = Some(event.state == PressState::Pressed);
        if is_modifier {
            // Modifier-only keys produce no PTY input but still update state.
            self.inspect_ring.push_modifiers(
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
            return None;
        }
        self.inspect_ring.push_key(
            &key_inspect_label(&event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            pressed,
        );
        let bytes = self.encode_key_with_kitty(&event)?;
        // Bounded encoding already ≤64; push respects MAX_PENDING_INPUT.
        self.push_input_bytes(&bytes);
        self.publish_inspect_snapshot();
        Some(bytes)
    }

    /// Convenience: handles a borrowed [`KeyEvent`] without moving it.
    pub fn handle_key_event_ref(&mut self, event: &KeyEvent) -> Option<Vec<u8>> {
        let is_modifier = matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(
                bitty_platform::NamedKey::Shift
                    | bitty_platform::NamedKey::Control
                    | bitty_platform::NamedKey::Alt
                    | bitty_platform::NamedKey::AltGraph
                    | bitty_platform::NamedKey::Super
                    | bitty_platform::NamedKey::Meta
            )
        );
        self.track_modifiers_from_key(event);
        // CTX-0166: any real non-modifier key press clears the selection
        // highlight (see owned path). Additive only; range logic untouched.
        if event.state == PressState::Pressed
            && !event.is_synthetic
            && !is_modifier
            && self.selection.is_some()
        {
            self.clear_selection();
        }
        // CTX-0186: Esc while a paste is pending cancels (see owned path).
        if self.cancel_pending_on_escape(event) {
            return None;
        }
        let pressed = Some(event.state == PressState::Pressed);
        if is_modifier {
            self.inspect_ring.push_modifiers(
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
            return None;
        }
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            pressed,
        );
        let bytes = self.encode_key_with_kitty(event)?;
        self.push_input_bytes(&bytes);
        self.publish_inspect_snapshot();
        Some(bytes)
    }

    /// Pushes raw input bytes into the pending queue and, when a PTY writer
    /// is live, writes them through.
    pub fn push_input_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // CTX-0176: with per-pane sessions live, input routes to the focused
        // leaf's shell only — never broadcast. Leaves without a session (the
        // primary leaf) keep the original writer path below.
        if !self.pane_sessions.is_empty() {
            self.push_input_bytes_multipane(bytes);
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
        self.buffer_input_headless(bytes);
    }

    /// Focused-leaf input routing for split layouts (CTX-0176): the focused
    /// leaf's session writer wins; the shared writer serves leaves without a
    /// session; with no writer live, bytes fall back to the bounded headless
    /// buffer. Best-effort, never panics.
    fn push_input_bytes_multipane(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        if let Some(focused) = self.focus.focused() {
            if let Some(sess) = self.pane_sessions.get_mut(&focused) {
                let _ = sess.writer.write_all(bytes);
                let _ = sess.writer.flush();
                return;
            }
        }
        if let Some(writer) = self.pty_writer.as_mut() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
            return;
        }
        self.buffer_input_headless(bytes);
    }

    /// Headless / no-writer input path: bounded buffer with drop-oldest.
    /// Shared by the single-pane and multipane routers so the bound
    /// (`MAX_PENDING_INPUT`, truncate-to-tail) stays identical on both.
    fn buffer_input_headless(&mut self, bytes: &[u8]) {
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
    // Selection and clipboard (CTX-0059) — winit/arboard with headless fallback
    // ------------------------------------------------------------------

    /// Current selection, if any (read-only).
    #[must_use]
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Whether a drag is in progress.
    #[must_use]
    pub fn is_selection_dragging(&self) -> bool {
        self.selection_dragging
    }

    /// Whether a selection currently exists and is non-empty.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    /// Clears the current selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_dragging = false;
        self.pending_full_redraw = true;
    }

    /// Directly sets the selection (headless test seam).
    pub fn set_selection(&mut self, selection: Selection) {
        let snap = self.state.snapshot();
        let clamped = selection.clamped(&snap).snapped(Some(&snap));
        self.selection = Some(clamped);
        self.selection_dragging = clamped.active;
        self.pending_full_redraw = true;
    }

    /// Starts a new selection at `pos` (mouse down).
    pub fn start_selection(&mut self, pos: CellPos) {
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        self.selection = Some(Selection {
            anchor: snapped,
            focus: snapped,
            kind: SelectionKind::Simple,
            active: true,
        });
        self.selection_dragging = true;
        self.pending_full_redraw = true;
    }

    /// Updates the current selection's focus to `pos` (mouse drag).
    pub fn update_selection(&mut self, pos: CellPos) {
        let Some(mut sel) = self.selection else {
            return;
        };
        if !self.selection_dragging {
            return;
        }
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        sel.focus = snapped;
        sel.active = true;
        self.selection = Some(sel);
        self.pending_full_redraw = true;
    }

    /// Ends the selection at `pos` (mouse up) and leaves it active for copy.
    pub fn end_selection(&mut self, pos: CellPos) {
        let Some(mut sel) = self.selection else {
            return;
        };
        let snap = self.state.snapshot();
        let clamped = clamp_cell_pos(&snap, pos);
        let snapped = bitty_ui::snap_to_leading(&snap, clamped);
        sel.focus = snapped;
        sel.active = false;
        self.selection_dragging = false;
        // Keep zero-length selections as None to avoid empty copies.
        if sel.anchor == sel.focus {
            self.selection = None;
        } else {
            self.selection = Some(sel);
        }
        self.pending_full_redraw = true;
    }

    /// Returns selected text for the current selection, if any.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let snap = self.state.snapshot();
        let text = sel.text(&snap);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Copies the current selection to the system clipboard (via the
    /// Wayland-first platform backend with headless fallback, which
    /// best-effort syncs the primary selection on Linux). Returns the copied
    /// text on success, `None` when no selection exists.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and the OS reports an error,
    /// returns `PlatformError::ClipboardOperation` but still updates the
    /// headless buffer so headless tests can observe the value.
    pub fn copy_selection_to_clipboard(
        &mut self,
    ) -> Result<Option<String>, bitty_platform::PlatformError> {
        let Some(text) = self.selection_text() else {
            return Ok(None);
        };
        self.clipboard.set_text(text.clone())?;
        Ok(Some(text))
    }

    /// Best-effort copy that never returns an error (drops system errors).
    pub fn copy_selection_lossy(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        self.clipboard.set_text_lossy(text.clone());
        Some(text)
    }

    /// Current contents of the platform primary (selection) clipboard buffer.
    ///
    /// Headless-first observation seam for tests: on a live Wayland/X11
    /// desktop this mirrors the last primary write through
    /// `bitty-platform::Clipboard`, so unit tests stay deterministic by
    /// forcing the headless clipboard first (`force_headless_clipboard`).
    #[must_use]
    pub fn primary_contents(&self) -> &str {
        self.clipboard.primary_contents()
    }

    /// Last clipboard failure observed on the mouse-paste path, if any.
    ///
    /// Mouse paste stays fail-soft (no bytes, no panic), but read/write
    /// failures from the platform clipboard are recorded here instead of
    /// swallowed, so the embedder can surface them (PR #259 review). A
    /// subsequent successful clipboard operation clears the slot. Cloned
    /// because [`Runtime`] is not `Sync`-friendly to borrow across frames.
    #[must_use]
    pub fn last_clipboard_error(&self) -> Option<bitty_platform::PlatformError> {
        self.last_clipboard_error.clone()
    }

    /// Records a platform clipboard failure for later surfacing.
    fn record_clipboard_error(&mut self, err: bitty_platform::PlatformError) {
        self.last_clipboard_error = Some(err);
    }

    /// Clears the recorded clipboard failure after a successful operation.
    fn clear_clipboard_error(&mut self) {
        self.last_clipboard_error = None;
    }

    /// Directly sets the platform primary clipboard (headless test seam).
    ///
    /// Routes through `bitty-platform::Clipboard::set_primary` (Wayland
    /// primary selection where supported, headless buffer otherwise).
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] but the headless buffer is still
    /// updated by the platform layer, so headless tests stay deterministic.
    pub fn set_primary_text(&mut self, text: String) {
        if let Err(err) = self.clipboard.set_primary(text) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
    }

    /// Copies the current selection to the platform primary clipboard.
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system error is recorded for
    /// [`Self::last_clipboard_error`] while the void return keeps the
    /// historic call shape.
    pub fn copy_selection_to_primary(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        if let Err(err) = self.clipboard.set_primary(text.clone()) {
            self.record_clipboard_error(err);
        } else {
            self.clear_clipboard_error();
        }
        Some(text)
    }

    /// Ghostty `setSelectionAndCopy` equivalent (CTX-0158): copies the
    /// current selection to the standard clipboard, which the platform layer
    /// best-effort syncs to the primary selection on Linux (CTX-0160).
    /// Returns the copied text, or `None` when no selection exists.
    /// Fail-soft: a system clipboard error is recorded for
    /// [`Self::last_clipboard_error`] while the headless buffers always
    /// update, and headless tests never touch the real clipboard.
    ///
    /// Called automatically on left-release only when
    /// `RuntimeConfig::selection_auto_copy` is `true` (CTX-0191); the explicit
    /// `copy_to_clipboard` chord calls the same path regardless of the toggle.
    pub fn auto_copy_selection(&mut self) -> Option<String> {
        let text = self.selection_text()?;
        match self.clipboard.set_text(text.clone()) {
            Ok(()) => self.clear_clipboard_error(),
            Err(err) => self.record_clipboard_error(err),
        }
        Some(text)
    }

    /// Pastes from the platform primary selection (middle-click /
    /// `wl-paste --primary`) through the same suspicious-paste inspection
    /// gate as clipboard input. Returns `None` when the primary selection is
    /// empty, otherwise `Some(true)` when the paste requires confirmation or
    /// `Some(false)` when delivered immediately.
    ///
    /// Fail-soft with a surfaced error: a platform read failure pastes
    /// nothing but is recorded for [`Self::last_clipboard_error`] instead of
    /// swallowed (PR #259 review); a successful read clears the slot.
    pub fn paste_from_primary(&mut self) -> Option<bool> {
        let text = match self.clipboard.get_primary() {
            Ok(text) => {
                self.clear_clipboard_error();
                text
            }
            Err(err) => {
                self.record_clipboard_error(err);
                return None;
            }
        };
        if text.is_empty() {
            return None;
        }
        Some(self.request_paste(text))
    }

    /// Whether the pending paste requires confirmation, if one exists.
    #[must_use]
    pub fn pending_paste_inspection(&self) -> Option<bool> {
        self.pending_paste
            .as_ref()
            .map(|p| p.inspection.needs_confirmation())
    }

    /// Whether a paste is awaiting confirmation.
    #[must_use]
    pub fn has_pending_paste(&self) -> bool {
        self.pending_paste.is_some()
    }

    /// Current pending paste text, if any.
    #[must_use]
    pub fn pending_paste_text(&self) -> Option<&str> {
        self.pending_paste.as_ref().map(|p| p.text.as_str())
    }

    /// Bounded human-readable summary of the pending paste, if any (CTX-0186,
    /// compacted CTX-0192).
    ///
    /// A gated paste is never silent: while [`Self::has_pending_paste`] holds,
    /// this returns `Some` single line of the form
    /// `Paste 2 lines, 11B [newline] "line1\nline2" (repeat=confirm Esc=cancel)`.
    ///
    /// Bounded and deterministic: the input is already capped at
    /// `CLIPBOARD_MAX_BYTES` (8192), reasons are at most 7 static tokens, and
    /// the preview keeps the first 32 chars escaped (`escape_debug`) and cut
    /// to 48 bytes at a char boundary. Total length stays well under 256
    /// bytes, single-line (no raw `\n`). `O(n)` with `n ≤ 8192`.
    #[must_use]
    pub fn pending_paste_summary(&self) -> Option<String> {
        let pending = self.pending_paste.as_ref()?;
        let lines = pending.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let bytes = pending.text.len();
        let reasons = pending.inspection.reasons().join(", ");
        let preview: String = pending.text.chars().take(32).collect();
        let preview = preview.escape_debug().to_string();
        let preview = truncate_str_to_bytes(&preview, 48);
        Some(format!(
            "Paste {lines} lines, {bytes}B [{reasons}] \"{preview}\" (repeat=confirm Esc=cancel)"
        ))
    }

    /// Whether the banner has collapsed to the minimal flash at `now`
    /// (CTX-0192). `None` when no paste pends.
    #[must_use]
    pub fn paste_banner_collapsed_at(&self, now: std::time::Instant) -> Option<bool> {
        self.pending_paste.as_ref()?;
        let since = self.pending_paste_since?;
        Some(now.saturating_duration_since(since) >= PASTE_BANNER_FULL_DURATION)
    }

    /// Visible banner text at `now` (CTX-0192): compact summary while fresh,
    /// [`PASTE_BANNER_FLASH_TEXT`] after [`PASTE_BANNER_FULL_DURATION`].
    /// Always `Some` while [`Self::has_pending_paste`] holds (never-silent),
    /// bounded, single-line, overlay-only.
    #[must_use]
    pub fn paste_banner_text_at(&self, now: std::time::Instant) -> Option<String> {
        if !self.has_pending_paste() {
            return None;
        }
        match self.paste_banner_collapsed_at(now) {
            Some(true) => Some(PASTE_BANNER_FLASH_TEXT.to_string()),
            _ => self.pending_paste_summary(),
        }
    }

    /// Visible banner text now (CTX-0192). See [`Self::paste_banner_text_at`].
    #[must_use]
    pub fn paste_banner_text(&self) -> Option<String> {
        self.paste_banner_text_at(std::time::Instant::now())
    }

    /// Pastes text from the system clipboard (or headless buffer) and routes
    /// it as terminal input via the bounded pending path. Returns
    /// `Err(PlatformError)` when clipboard acquisition fails, `Ok(None)` when
    /// the clipboard is empty, and `Ok(Some(true))` when confirmation is
    /// required or `Ok(Some(false))` when the text is delivered immediately.
    ///
    /// The right-click mouse path records `Err` for
    /// [`Self::last_clipboard_error`] instead of dropping it (PR #259
    /// review); direct callers match on the `Result` themselves.
    ///
    /// Suspicious-paste inspection (P0-AC-008): every paste is inspected for
    /// C0/NUL/ESC/CR/newline/Unicode BiDi controls. Clean text is delivered
    /// immediately; suspicious text is stored as a pending paste that requires
    /// explicit confirmation — `confirm_pending_paste(true)`, repeating the
    /// identical paste while pending (CTX-0186 second chord/right-click press
    /// with unchanged clipboard), or `Esc` to cancel. The pending paste stays
    /// visible via [`Self::pending_paste_summary`]: there is no silent
    /// delivery path and no silent drop. Bracketed paste (`?2004`) is
    /// defense-in-depth only and wraps confirmed delivery when enabled in
    /// terminal state.
    ///
    /// Paste is bounded to `CLIPBOARD_MAX_BYTES` (8192) via the clipboard
    /// primitive before the scan, so untrusted clipboard content cannot grow
    /// the heap without limit (T-01).
    pub fn paste_from_clipboard(&mut self) -> Result<Option<bool>, bitty_platform::PlatformError> {
        let text = self.clipboard.get_text()?;
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.request_paste(text)))
    }

    /// Pastes from a given string via the inspection gate (headless helper).
    /// Returns `true` when the submitted paste requires confirmation and
    /// `false` when it is delivered immediately. Re-submitting the identical
    /// pending text confirms and delivers (CTX-0186); different suspicious
    /// content while pending preserves the first paste and returns `true`.
    pub fn paste_text_via_gate(&mut self, text: String) -> bool {
        self.request_paste(text)
    }

    /// Pastes the given text through the suspicious-paste inspection gate.
    ///
    /// This string-input seam is safe for production callers because it uses
    /// the same pending confirmation path as clipboard input.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.request_paste(text.to_owned());
    }

    /// Core paste entry: bounds and inspects `text`, stores a pending paste
    /// when suspicious, otherwise delivers immediately. Returns `true` when
    /// confirmation is required and `false` when delivery is immediate. A
    /// different suspicious request while another paste is pending is rejected,
    /// which preserves the first pending paste for explicit confirmation or
    /// cancel.
    ///
    /// CTX-0186 explicit repeat-to-confirm: re-submitting the identical
    /// (post-truncation) text while it is pending is the user's confirmation
    /// gesture — the second chord/right-click press with an unchanged
    /// clipboard delivers (bracketed when `?2004` is on) and clears pending.
    /// Different content while pending preserves the first paste (TOCTOU-safe:
    /// a swapped clipboard cannot smuggle new bytes through confirmation).
    ///
    /// No silent delivery path exists for `needs_confirmation() == true`.
    pub fn request_paste(&mut self, text: String) -> bool {
        let text = truncate_paste_text(text);
        // Explicit confirmation: identical re-paste while pending delivers.
        if let Some(pending) = self.pending_paste.as_ref() {
            if pending.text == text {
                let pending = self.pending_paste.take().expect("checked above");
                self.deliver_paste_bytes_bracketed(&pending.text);
                self.pending_paste_since = None;
                self.paste_banner_collapsed = false;
                self.pending_full_redraw = true;
                return false;
            }
        }
        let inspection = crate::paste::inspect_paste(&text);
        if inspection.needs_confirmation() {
            if self.pending_paste.is_some() {
                return true;
            }
            self.pending_paste = Some(crate::paste::PendingPaste::new(text, inspection.clone()));
            // CTX-0192 transient banner starts full now.
            self.pending_paste_since = Some(std::time::Instant::now());
            self.paste_banner_collapsed = false;
            self.pending_full_redraw = true;
            return true;
        }
        self.deliver_paste_bytes(text.as_bytes());
        false
    }

    /// Confirm or cancel the pending paste. `confirm == true` delivers the
    /// pending text (bracketed when `?2004` is enabled); `false` drops it.
    ///
    /// Returns `true` when a pending paste existed and was handled.
    pub fn confirm_pending_paste(&mut self, confirm: bool) -> bool {
        let Some(pending) = self.pending_paste.take() else {
            return false;
        };
        if confirm {
            self.deliver_paste_bytes_bracketed(&pending.text);
        }
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        true
    }

    /// Cancel any pending paste without delivery.
    pub fn cancel_pending_paste(&mut self) -> bool {
        self.confirm_pending_paste(false)
    }

    /// Consume an `Esc` press while a paste is pending (CTX-0186).
    ///
    /// Returns `true` when the event was an `Esc` press with a pending paste:
    /// the pending paste is dropped without delivery, a redraw is requested so
    /// any pending indicator clears, and the caller must not forward the key
    /// to the PTY. Returns `false` otherwise (no pending paste, not `Esc`, or
    /// not a press), leaving existing key routing untouched.
    fn cancel_pending_on_escape(&mut self, event: &KeyEvent) -> bool {
        if event.state != PressState::Pressed {
            return false;
        }
        if !matches!(
            &event.logical_key,
            bitty_platform::LogicalKey::Named(bitty_platform::NamedKey::Escape)
        ) {
            return false;
        }
        if self.pending_paste.is_none() {
            return false;
        }
        self.pending_paste = None;
        self.pending_paste_since = None;
        self.paste_banner_collapsed = false;
        self.pending_full_redraw = true;
        self.inspect_ring.push_key(
            &key_inspect_label(event),
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
            Some(true),
        );
        self.publish_inspect_snapshot();
        true
    }

    fn deliver_paste_bytes(&mut self, bytes: &[u8]) {
        self.write_input(bytes);
    }

    fn deliver_paste_bytes_bracketed(&mut self, text: &str) {
        let bracketed = self.state.modes().bracketed_paste;
        let bytes = crate::paste::bracketed_wrap(text, bracketed);
        self.write_input(&bytes);
    }

    /// Selects all cells in the current snapshot (Ctrl+Shift+A / triple-click equivalent).
    pub fn select_all(&mut self) {
        let snap = self.state.snapshot();
        if snap.width == 0 || snap.height == 0 {
            self.selection = None;
            self.pending_full_redraw = true;
            return;
        }
        let start = CellPos::new(0, 0);
        let end = CellPos::new((snap.height - 1) as u16, (snap.width - 1) as u16);
        let sel = Selection {
            anchor: start,
            focus: bitty_ui::snap_to_leading(&snap, end),
            kind: SelectionKind::Simple,
            active: false,
        };
        self.selection = Some(sel);
        self.selection_dragging = false;
        self.pending_full_redraw = true;
    }

    /// Converts a physical cursor position to a grid cell coordinate using
    /// the live (DPI-scaled) cell metrics. Clamped to the current snapshot bounds.
    ///
    /// When the position lies far outside the window it clamps to the nearest
    /// cell rather than returning `None`, so drag selections that leave the
    /// window still produce deterministic inclusive ranges.
    #[must_use]
    pub fn cursor_to_cell(&self, pos: CursorPosition) -> CellPos {
        let snap = self.state.snapshot();
        let live = self.live_cell_metrics();
        let cell_w = live.width as f64;
        let cell_h = live.height as f64;
        let col = if cell_w <= 0.0 {
            0
        } else {
            (pos.x / cell_w).floor() as i64
        };
        let row = if cell_h <= 0.0 {
            0
        } else {
            (pos.y / cell_h).floor() as i64
        };
        let max_col = snap.width.saturating_sub(1) as i64;
        let max_row = snap.height.saturating_sub(1) as i64;
        let clamped_col = col.clamp(0, max_col) as u16;
        let clamped_row = row.clamp(0, max_row) as u16;
        bitty_ui::snap_to_leading(&snap, CellPos::new(clamped_row, clamped_col))
    }

    /// Handles a mouse button event for selection or terminal mouse tracking.
    ///
    /// Single-window vertical slice semantics (candidate Input RFC, ghostty
    /// reference `recordings/references/ghostty/src/Surface.zig`):
    /// - When mouse tracking is enabled (`1000`/`1002`/`1003`) + SGR `1006` and
    ///   not in shift-override, every button event encodes to bounded SGR bytes
    ///   (`ESC[<b;x;yM/m`, ≤32 bytes) and is written to the PTY. Right/middle
    ///   paste never fires in this path.
    /// - Holding Shift bypasses capture unconditionally to force selection
    ///   (accessibility escape).
    /// - Otherwise the event drives presentation selection with ghostty
    ///   copy-on-select: left press starts a drag, left release commits it and
    ///   auto-copies to the platform clipboard (which best-effort syncs the
    ///   primary selection on Linux, CTX-0160) — unless
    ///   `RuntimeConfig::selection_auto_copy` is `false` (CTX-0191 opt-out:
    ///   the highlight stays and only the explicit `copy_to_clipboard` chord
    ///   copies); right press pastes the
    ///   standard clipboard (Wayland-first backend) and middle press pastes
    ///   the platform primary selection, both through the suspicious-paste
    ///   inspection gate. All three stay fail-soft (empty source means no
    ///   bytes, never a panic or block) but platform failures are recorded
    ///   for [`Self::last_clipboard_error`] instead of swallowed.
    pub fn handle_mouse_input(&mut self, event: bitty_platform::MouseEvent) {
        // CTX-0159: retain a bounded mouse trace for screenshots-free probes.
        // Coordinates come from the last known cursor position mapped to cell
        // space (clamped); `None` when the cursor never entered the window.
        {
            let cell = self.last_cursor.map(|pos| self.cursor_to_cell(pos));
            let button = match event.button {
                MouseButton::Left => "Left",
                MouseButton::Right => "Right",
                MouseButton::Middle => "Middle",
                MouseButton::Back => "Back",
                MouseButton::Forward => "Forward",
                MouseButton::Other(_) => "Other",
            };
            let pressed = event.state == PressState::Pressed;
            self.inspect_ring.push_mouse(
                button,
                cell.map(|c| c.col),
                cell.map(|c| c.row),
                pressed,
                self.shift_pressed,
                self.control_pressed,
                self.alt_pressed,
            );
            self.publish_inspect_snapshot();
        }
        // Shift override always forces selection path.
        let shift_override = self.shift_pressed;
        let capture = !shift_override
            && self.state.modes().mouse_tracking.is_some()
            && self.state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Sgr)
            && self.should_capture_mouse();

        if capture {
            if let Some(pos) = self.last_cursor {
                let cell = self.cursor_to_cell(pos);
                // Bounded SGR encoding: ≤32 bytes per event, batch ≤4 KiB (candidate)
                // Coordinates are 1-based per SGR, clamped to [1,65535] then to grid.
                let col = (cell.col as u32 + 1).clamp(1, 65535) as u16;
                let row = (cell.row as u32 + 1).clamp(1, 65535) as u16;
                let mut code = match event.button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 9,
                    MouseButton::Other(n) => (n % 8) as u8,
                };
                // Modifier bits: shift 4, alt 8, ctrl 16
                if self.shift_pressed {
                    code |= 4;
                }
                if self.alt_pressed {
                    code |= 8;
                }
                if self.control_pressed {
                    code |= 16;
                }
                // Drag adds 32 for button-event tracking? For simplicity we map release vs press.
                let trailer = if event.state == PressState::Pressed {
                    'M'
                } else {
                    'm'
                };
                // For SGR, release is still reported with same button code but 'm'
                // Bounded <32 bytes: format "ESC[<code;col;rowM"
                let seq = format!("\x1b[<{code};{col};{row}{trailer}");
                // Enforce bound explicitly (candidate table: mouse encode ≤32 bytes)
                let bytes = if seq.len() > 32 {
                    &seq.as_bytes()[..32]
                } else {
                    seq.as_bytes()
                };
                self.push_input_bytes(bytes);
            }
            // Still update last_cursor tracking but do not start selection.
            // CTX-0166: a captured click must still dismiss any stale
            // highlight so the gray rect never lingers while a mouse-mode app
            // owns the pointer. Additive clearing only; range logic untouched.
            if self.selection.is_some() {
                self.clear_selection();
            }
            return;
        }

        // Selection path (including shift override)
        match (event.button, event.state) {
            (MouseButton::Left, PressState::Pressed) => {
                if let Some(pos) = self.last_cursor {
                    let cell = self.cursor_to_cell(pos);
                    self.start_selection(cell);
                } else if self.selection.is_some() {
                    // CTX-0166: click without cursor tracking still dismisses
                    // the highlight (no stale rect when `last_cursor` is None).
                    self.clear_selection();
                }
            }
            (MouseButton::Left, PressState::Released) => {
                if let Some(pos) = self.last_cursor {
                    let cell = self.cursor_to_cell(pos);
                    self.end_selection(cell);
                } else {
                    self.selection_dragging = false;
                    if let Some(mut sel) = self.selection {
                        sel.active = false;
                        self.selection = Some(sel);
                    }
                    self.pending_full_redraw = true;
                }
                // Ghostty copy-on-select: a committed drag auto-copies to
                // both selections via the platform clipboard (Wayland-first,
                // CTX-0160) — unless `selection.auto_copy` is false (CTX-0191
                // opt-out: the highlight stays and only the explicit
                // `copy_to_clipboard` chord copies). Fail-soft: empty
                // selection pastes nothing and a system clipboard error is
                // recorded for `last_clipboard_error` while the headless
                // buffers still update; the input path never blocks.
                if self.config.selection_auto_copy {
                    let _ = self.auto_copy_selection();
                }
            }
            (MouseButton::Right, PressState::Pressed) => {
                // Ghostty `paste` right-click action for the standard
                // clipboard (Wayland-first via the platform backend,
                // CTX-0160). Fail-soft: empty clipboards paste nothing and
                // suspicious text waits on the confirmation gate; a read
                // failure is recorded for `last_clipboard_error` instead of
                // swallowed (PR #259 review).
                match self.paste_from_clipboard() {
                    Ok(_) => self.clear_clipboard_error(),
                    Err(err) => self.record_clipboard_error(err),
                }
            }
            (MouseButton::Middle, PressState::Pressed) => {
                // Ghostty `primary-paste` middle-click action for the
                // platform primary selection (fail-soft like right-click;
                // read failures are recorded inside `paste_from_primary`).
                let _ = self.paste_from_primary();
            }
            _ => {}
        }
    }

    fn should_capture_mouse(&self) -> bool {
        // Capture when a mouse mode is enabled; for single-window slice we
        // capture in any view (not only alternate screen) to prove 1000..1006
        // end-to-end, but we document that alternate-screen capture is the
        // normative owner. This keeps headless tests deterministic.
        self.state.modes().mouse_tracking.is_some()
    }

    /// Handles cursor movement for drag selection or mouse-tracking motion.
    pub fn handle_cursor_moved(&mut self, pos: CursorPosition) {
        self.last_cursor = Some(pos);
        if self.selection_dragging {
            let cell = self.cursor_to_cell(pos);
            self.update_selection(cell);
            return;
        }
        // Motion reporting for 1003 (Any) or 1002 drag: encode as motion when capture active.
        let capture = !self.shift_pressed
            && self.state.modes().mouse_tracking == Some(bitty_vt::MouseTrackingMode::Any)
            && self.state.modes().mouse_coordinate_encoding
                == Some(bitty_vt::MouseCoordinateEncoding::Sgr);
        if capture {
            let cell = self.cursor_to_cell(pos);
            let col = (cell.col as u32 + 1).clamp(1, 65535) as u16;
            let row = (cell.row as u32 + 1).clamp(1, 65535) as u16;
            // Motion button code 32 (no button) plus modifiers, SGR uses 'M' for press/drag
            let mut code: u8 = 32;
            if self.shift_pressed {
                code |= 4;
            }
            if self.alt_pressed {
                code |= 8;
            }
            if self.control_pressed {
                code |= 16;
            }
            let seq = format!("\x1b[<{code};{col};{row}M");
            let bytes = if seq.len() > 32 {
                &seq.as_bytes()[..32]
            } else {
                seq.as_bytes()
            };
            // Bounded: drop if PTY queue full, never block
            self.push_input_bytes(bytes);
        }
    }

    /// Handles wheel scroll: accumulates line-notch and pixel deltas and
    /// emits lines or scrolls the viewport, scaled by the configured scroll
    /// speed (`RuntimeConfig::scroll_lines_per_notch` /
    /// `scroll_pixels_per_notch`).
    ///
    /// CTX-0185 profile (vs ghostty on the same machine): the lag was
    /// throughput, not redundant work. The `Lines` path moved exactly 1 line
    /// per notch (`y as isize`) while ghostty-class terminals move 3, and
    /// fractional `Lines` deltas (`|y| < 1.0` from high-resolution wheels)
    /// truncated to zero and were dropped outright. Scheduling was already
    /// coalesced (wheel events only set `pending_full_redraw`; one `tick`
    /// per event-loop pass presents, so an N-event fling costs one full
    /// redraw), and the full redraw itself is required — every viewport cell
    /// changes under scroll. The fix is speed plus a fractional accumulator,
    /// not fewer presents.
    ///
    /// Direction semantics (CTX-0155) are untouched: `y > 0` is up into
    /// history on both paths.
    /// Bounded: at most 32 lines per frame per path; the line-notch
    /// accumulator is clamped to one frame cap and the pixel accumulator to
    /// 4x the notch threshold, so a spinning wheel cannot bank unbounded
    /// drift.
    #[allow(clippy::unnecessary_cast)]
    pub fn handle_wheel(&mut self, delta: ScrollDelta) {
        // CTX-0159: retain a bounded wheel trace for screenshots-free probes.
        match delta {
            ScrollDelta::Lines(x, y) => {
                self.inspect_ring.push_wheel(
                    (x as i32).clamp(-32, 32),
                    (y as i32).clamp(-32, 32),
                    self.shift_pressed,
                    self.control_pressed,
                    self.alt_pressed,
                );
            }
            ScrollDelta::Pixels(px, py) => {
                self.inspect_ring.push_wheel(
                    (px as i32).clamp(-512, 512),
                    (py as i32).clamp(-512, 512),
                    self.shift_pressed,
                    self.control_pressed,
                    self.alt_pressed,
                );
            }
        }
        self.publish_inspect_snapshot();
        // Validated `1..=` at construction; `max(1)` keeps release builds
        // total even if a struct-literal config ever bypasses validation.
        let lines_per_notch = self.config.scroll_lines_per_notch.max(1) as f32;
        let pixels_per_notch = self.config.scroll_pixels_per_notch.max(1) as f32;
        match delta {
            ScrollDelta::Lines(x, y) => {
                // Scale notches into lines and bank the fraction so
                // sub-notch deltas survive across events instead of
                // truncating to zero.
                self.wheel_line_accum_y =
                    (self.wheel_line_accum_y + y * lines_per_notch).clamp(-32.0, 32.0);
                self.wheel_line_accum_x =
                    (self.wheel_line_accum_x + x * lines_per_notch).clamp(-32.0, 32.0);
                let lines_y = self.wheel_line_accum_y.trunc() as isize;
                let lines_x = self.wheel_line_accum_x.trunc() as isize;
                // Shift+wheel or no capture scrolls viewport; otherwise emit mouse wheel SGR when mouse mode active.
                let capture_scroll = !self.shift_pressed
                    && self.state.modes().mouse_tracking.is_some()
                    && self.state.modes().mouse_coordinate_encoding
                        == Some(bitty_vt::MouseCoordinateEncoding::Sgr);
                if capture_scroll {
                    // SGR wheel: buttons 64 (up) / 65 (down), horizontal 66/67
                    for _ in 0..lines_y.abs().min(32) {
                        let btn = if lines_y > 0 { 64 } else { 65 };
                        let seq = if let Some(pos) = self.last_cursor {
                            let cell = self.cursor_to_cell(pos);
                            let col = (cell.col as u32 + 1) as u16;
                            let row = (cell.row as u32 + 1) as u16;
                            format!("\x1b[<{btn};{col};{row}M")
                        } else {
                            format!("\x1b[<{btn};1;1M")
                        };
                        self.push_input_bytes(seq.as_bytes());
                    }
                    for _ in 0..lines_x.unsigned_abs().min(32) {
                        let btn = if lines_x > 0 { 66 } else { 67 };
                        let seq = format!("\x1b[<{btn};1;1M");
                        self.push_input_bytes(seq.as_bytes());
                    }
                    self.wheel_line_accum_y -= lines_y as f32;
                    self.wheel_line_accum_x -= lines_x as f32;
                } else {
                    // Horizontal has no viewport meaning; drain it so no
                    // stale fraction survives into a later capture session.
                    self.wheel_line_accum_x = 0.0;
                    // Viewport scroll
                    if lines_y != 0 {
                        // winit LineDelta y>0 = wheel up; View::scroll_by
                        // positive = up into history, so delta is +lines.
                        let max = self.state.scrollback_len();
                        if let Some(view_id) = self.focused_view() {
                            if let Some(view) = self.layout.find_leaf_mut(view_id) {
                                view.scroll_by(lines_y, max);
                            }
                        } else {
                            // Single-window fallback: find leaf 1
                            if let Some(view) = self.layout.find_leaf_mut(ViewId::new(1)) {
                                view.scroll_by(lines_y, max);
                            }
                        }
                        self.wheel_line_accum_y -= lines_y as f32;
                        self.pending_full_redraw = true;
                    }
                }
            }
            ScrollDelta::Pixels(px, py) => {
                // Accumulate pixel deltas; threshold = configured pixels
                // per notch (default 16 = one default cell height).
                self.wheel_accum_x += px as f32;
                self.wheel_accum_y += py as f32;
                let bound = 4.0 * pixels_per_notch;
                self.wheel_accum_y = self.wheel_accum_y.clamp(-bound, bound);
                self.wheel_accum_x = self.wheel_accum_x.clamp(-bound, bound);
                let notches_y = self.wheel_accum_y / pixels_per_notch;
                let notches_x = self.wheel_accum_x / pixels_per_notch;
                if notches_y.trunc() != 0.0 || notches_x.trunc() != 0.0 {
                    // Use lines path with coalescing; the lines multiplier
                    // applies once, inside the Lines path.
                    let clamped_y = notches_y.clamp(-32.0, 32.0);
                    let clamped_x = notches_x.clamp(-32.0, 32.0);
                    self.handle_wheel(ScrollDelta::Lines(clamped_x, clamped_y));
                    self.wheel_accum_y -= clamped_y * pixels_per_notch;
                    self.wheel_accum_x -= clamped_x * pixels_per_notch;
                }
            }
        }
    }

    /// Tracks modifier state from keyboard events (Shift/Ctrl/Alt).
    pub fn track_modifiers_from_key(&mut self, event: &KeyEvent) {
        // Update shift/ctrl/alt pressed state based on named keys.
        // This keeps hot-path allocation-free (no HashMap) and bounded.
        if let bitty_platform::LogicalKey::Named(named) = &event.logical_key {
            match named {
                bitty_platform::NamedKey::Shift => {
                    self.shift_pressed = event.state == PressState::Pressed;
                }
                bitty_platform::NamedKey::Control => {
                    self.control_pressed = event.state == PressState::Pressed;
                }
                bitty_platform::NamedKey::Alt | bitty_platform::NamedKey::AltGraph => {
                    self.alt_pressed = event.state == PressState::Pressed;
                }
                _ => {}
            }
        }
    }

    /// Sets focus state and emits focus reports when mode 1004 is enabled.
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        let gained = focused;
        self.focused = focused;
        // CTX-0159: retain focus transitions for screenshots-free probes.
        self.inspect_ring.push_focus(
            focused,
            self.shift_pressed,
            self.control_pressed,
            self.alt_pressed,
        );
        if gained {
            self.pending_full_redraw = true;
        }
        if self.state.modes().focus_events {
            let seq = if focused { "\x1b[I" } else { "\x1b[O" };
            self.push_input_bytes(seq.as_bytes());
        }
        self.publish_inspect_snapshot();
    }

    /// Handles IME preedit (presentation overlay, not Terminal Truth).
    pub fn handle_ime_preedit(&mut self, text: Option<String>, cursor: Option<usize>) {
        // Bounded: preedit ≤128 chars or 256 bytes per candidate TXT-10/11; truncate at char boundary.
        if let Some(t) = text {
            const MAX: usize = 128;
            let truncated = if t.chars().count() > MAX {
                let mut s = String::new();
                for (i, ch) in t.chars().enumerate() {
                    if i >= MAX {
                        break;
                    }
                    s.push(ch);
                }
                s
            } else {
                t
            };
            let cur = cursor.unwrap_or(truncated.len()).min(truncated.len());
            self.ime_preedit = Some(truncated);
            self.ime_cursor = cur;
            self.pending_full_redraw = true;
        } else {
            self.ime_preedit = None;
            self.ime_cursor = 0;
            self.pending_full_redraw = true;
        }
    }

    /// Commits IME text: bounded ≤256 chars / ≤1024 bytes, then encoder path.
    #[allow(clippy::explicit_counter_loop)]
    pub fn handle_ime_commit(&mut self, text: String) {
        // Bounded before allocation (candidate TXT-11)
        let bytes_len = text.len();
        let char_count = text.chars().count();
        let bounded = if bytes_len > 1024 || char_count > 256 {
            let mut out = String::new();
            let mut bytes = 0usize;
            let mut chars = 0usize;
            for ch in text.chars() {
                let clen = ch.len_utf8();
                if bytes + clen > 1024 || chars + 1 > 256 {
                    break;
                }
                out.push(ch);
                bytes += clen;
                chars += 1;
            }
            out
        } else {
            text
        };
        self.ime_preedit = None;
        self.ime_cursor = 0;
        // CTX-0166: IME commit is typing — dismiss the highlight first so the
        // rect never lingers a frame past the state. `clear_selection` forces
        // the next tick to present; the final flag below keeps that promise.
        if self.selection.is_some() {
            self.clear_selection();
        }
        // IME commit shares PTY write queue with keyboard (bounded 8192)
        self.push_input_bytes(bounded.as_bytes());
        self.pending_full_redraw = true;
    }

    /// Current cursor position, if known.
    #[must_use]
    pub fn last_cursor(&self) -> Option<CursorPosition> {
        self.last_cursor
    }

    /// Owned clipboard handle (mutable) for advanced use (e.g. OSC 52 tests).
    pub fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// Owned clipboard handle (read-only).
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// Forces the clipboard into headless mode (test helper, deterministic).
    ///
    /// Replaces the handle (clearing both the standard and primary headless
    /// buffers) and drops any recorded clipboard error, so tests start from
    /// a clean seam and never touch the real clipboard or primary.
    pub fn force_headless_clipboard(&mut self) {
        self.clipboard = Clipboard::new_headless();
        self.last_clipboard_error = None;
    }

    /// Allow or deny OSC 52 clipboard writes (capability-gated, default false).
    pub fn set_osc_clipboard_write_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_write_allowed = allowed;
    }

    /// Allow or deny OSC 52 clipboard reads / queries (consent-gated, default false).
    pub fn set_osc_clipboard_read_allowed(&mut self, allowed: bool) {
        self.osc_clipboard_read_allowed = allowed;
    }

    /// Whether OSC 52 writes are currently allowed.
    #[must_use]
    pub fn osc_clipboard_write_allowed(&self) -> bool {
        self.osc_clipboard_write_allowed
    }

    /// Whether OSC 52 reads are currently allowed (consent-gated).
    #[must_use]
    pub fn osc_clipboard_read_allowed(&self) -> bool {
        self.osc_clipboard_read_allowed
    }

    // ------------------------------------------------------------------
    // Scrollback search and selection persistence (CTX-0060) — headless
    // ------------------------------------------------------------------

    /// Searches scrollback and live grid for `pattern`.
    ///
    /// Bounded by [`bitty_term_state::search::SEARCH_MAX_PATTERN_LEN`] and
    /// [`bitty_term_state::search::SEARCH_MAX_RESULTS`]; headless and deterministic;
    /// no I/O. Delegates to [`State::search`].
    #[must_use]
    pub fn search(&self, pattern: &str, options: SearchOptions) -> Vec<SearchMatch> {
        self.state.search(pattern, options)
    }

    /// Convenience: case-sensitive search with default limits.
    #[must_use]
    pub fn search_case_sensitive(&self, pattern: &str) -> Vec<SearchMatch> {
        self.search(pattern, SearchOptions::default())
    }

    /// Lifts the current live-grid selection to a buffer-anchored persistent
    /// selection, if any. The returned value survives scroll (lines moving
    /// from grid into scrollback), `View` scroll offset changes, and resize
    /// (clamped). Returns `None` when no selection exists.
    #[must_use]
    pub fn persistent_selection(&self) -> Option<PersistentSelection> {
        let sel = self.selection?;
        Some(PersistentSelection::from_grid_selection(sel, &self.state))
    }

    /// Attempts to restore a persistent selection into the live-grid selection.
    ///
    /// Returns `true` when the persistent buffer rows still map into the
    /// current live grid window (and survive pruning); `false` when the
    /// selection has moved into history or been pruned. On `false` the live
    /// selection is cleared to keep invariants (empty pruned selections never
    /// linger as stale grid coords). Headless and bounded.
    pub fn restore_persistent_selection(&mut self, pers: PersistentSelection) -> bool {
        if let Some(sel) = pers.to_grid_selection(&self.state) {
            self.selection = Some(sel);
            self.selection_dragging = sel.active;
            true
        } else {
            // Buffer is either pruned or now in history: clear live selection.
            // Caller may still use `pers.text(&state)` for history highlight.
            self.selection = None;
            self.selection_dragging = false;
            false
        }
    }

    /// Returns the buffer text for a persistent selection, if still valid.
    ///
    /// This reads from scrollback + live grid according to the persistent
    /// buffer rows, so a selection that has scrolled into history still yields
    /// its original text (unless pruned). Headless.
    #[must_use]
    pub fn persistent_selection_text(&self, pers: &PersistentSelection) -> Option<String> {
        pers.text(&self.state)
    }

    /// Whether a persistent selection is still valid against the current state
    /// (not pruned, buffer rows in bounds).
    #[must_use]
    pub fn is_persistent_selection_valid(&self, pers: &PersistentSelection) -> bool {
        pers.is_valid(&self.state)
    }

    /// View-aware persistence: lifts a viewport `Selection` (viewport rows) to
    /// a persistent selection anchored to the combined buffer (respects `View`
    /// scroll offset). Headless.
    #[must_use]
    pub fn persistent_selection_from_view(
        &self,
        sel: Selection,
        view: &View,
    ) -> PersistentSelection {
        PersistentSelection::from_view_selection(sel, view, &self.state)
    }

    /// View-aware restore: attempts to map a persistent selection back into a
    /// viewport `Selection` for the given `View`. Returns `None` when the
    /// selection is outside the current viewport window or pruned.
    #[must_use]
    pub fn persistent_to_view_selection(
        &self,
        pers: &PersistentSelection,
        view: &View,
    ) -> Option<Selection> {
        pers.to_view_selection(view, &self.state)
    }

    // ------------------------------------------------------------------
    // Scrollback search UI integration (CTX-0061) — headless
    // ------------------------------------------------------------------

    /// Owned search UI state (read-only).
    ///
    /// `SearchState` owns the bounded query (`≤256` bytes), options, bounded
    /// matches (`≤1000`), and the current navigation index. All operations are
    /// headless, bounded, and deterministic: `search_set` truncates the
    /// pattern, `State::search` caps results, navigation wraps, and view
    /// highlight mapping is pure arithmetic.
    #[must_use]
    pub fn search_state(&self) -> &SearchState {
        &self.search_state
    }

    /// Owned search UI state (mutable, for tests).
    #[must_use]
    pub fn search_state_mut(&mut self) -> &mut SearchState {
        &mut self.search_state
    }

    /// Sets the search query and recomputes bounded matches against the live state.
    ///
    /// Bounded by [`bitty_term_state::search::SEARCH_MAX_PATTERN_LEN`] and
    /// [`bitty_term_state::search::SEARCH_MAX_RESULTS`]; headless and deterministic.
    /// The UI becomes active iff the truncated pattern is non-empty and
    /// `options.max_results != 0`. When matches are non-empty `current` is set to
    /// `Some(0)`, otherwise cleared. Does not touch `selection` automatically;
    /// call [`Self::search_apply_selection`] to move the live selection to the
    /// current match when desired (selection-persistence integration).
    pub fn search_set(&mut self, pattern: &str, options: SearchOptions) {
        self.search_state.set_search(&self.state, pattern, options);
    }

    /// Clears the search UI (pattern empty, matches cleared, inactive).
    pub fn search_clear(&mut self) {
        self.search_state.clear();
    }

    /// Refreshes the current search against the live state after scrollback
    /// growth, resize, or new input. Preserves `current` clamped to the new
    /// match count (or `None` when empty). No-op when search is inactive.
    pub fn search_refresh(&mut self) {
        self.search_state.refresh(&self.state);
    }

    /// Advances to the next match (wraps deterministically).
    pub fn search_next(&mut self) {
        self.search_state.next();
    }

    /// Advances to the previous match (wraps deterministically).
    pub fn search_prev(&mut self) {
        self.search_state.prev();
    }

    /// Advances the search by `delta` with wrapping.
    pub fn search_advance(&mut self, delta: isize) {
        self.search_state.advance(delta);
    }

    /// Number of matches for the current query (≤ [`bitty_term_state::search::SEARCH_MAX_RESULTS`]).
    #[must_use]
    pub fn search_match_count(&self) -> usize {
        self.search_state.match_count()
    }

    /// Whether the search UI is active (non-empty pattern and max_results > 0).
    #[must_use]
    pub fn search_is_active(&self) -> bool {
        self.search_state.is_active()
    }

    /// Current query pattern (truncated to `SEARCH_MAX_PATTERN_LEN`).
    #[must_use]
    pub fn search_pattern(&self) -> &str {
        self.search_state.pattern()
    }

    /// Current search options (clamped).
    #[must_use]
    pub fn search_options(&self) -> SearchOptions {
        self.search_state.options()
    }

    /// Current match index, if any.
    #[must_use]
    pub fn search_current_index(&self) -> Option<usize> {
        self.search_state.current_index()
    }

    /// Current match, if any.
    #[must_use]
    pub fn search_current_match(&self) -> Option<&SearchMatch> {
        self.search_state.current_match()
    }

    /// Bounded matches for the current query (ordered oldest-scrollback-first).
    #[must_use]
    pub fn search_matches(&self) -> &[SearchMatch] {
        self.search_state.matches()
    }

    /// Persistent selection that exactly spans the current match, if any.
    ///
    /// This is the selection-persistence integration point: the returned
    /// `PersistentSelection` survives scroll (including history) and resize
    /// (clamped), and its `is_valid` tracks pruning. When the match is in the
    /// live grid `to_grid_selection` succeeds; when in history the text is
    /// still readable via `pers.text(&state)`.
    #[must_use]
    pub fn search_current_persistent_selection(&self) -> Option<PersistentSelection> {
        self.search_state.current_persistent_selection(&self.state)
    }

    /// Persistent selection for match `idx`, if in bounds and still valid.
    #[must_use]
    pub fn search_match_persistent_selection(&self, idx: usize) -> Option<PersistentSelection> {
        self.search_state
            .match_persistent_selection(&self.state, idx)
    }

    /// All current matches as bounded persistent selections (≤ `SEARCH_MAX_RESULTS`).
    ///
    /// Each entry is `is_valid`-filtered, so pruned history matches are dropped
    /// deterministically.
    #[must_use]
    pub fn search_all_persistent_selections(&self) -> Vec<PersistentSelection> {
        self.search_state.all_persistent_selections(&self.state)
    }

    /// Indices of matches whose `buffer_row` is currently visible in `view`.
    #[must_use]
    pub fn search_visible_match_indices(&self, view: &View) -> Vec<usize> {
        self.search_state.visible_match_indices(view, &self.state)
    }

    /// Highlights for matches currently visible in `view`, with view-local
    /// coordinates and `is_current` flag.
    ///
    /// Headless helper for the renderer: maps each visible `SearchMatch` to its
    /// `view_row`, `view_col_start..view_col_end`, and whether it is the
    /// current navigated target.
    #[must_use]
    pub fn search_visible_highlights(&self, view: &View) -> Vec<SearchHighlight> {
        self.search_state.visible_highlights(view, &self.state)
    }

    /// Scrolls `view` vertically (and horizontally when needed) to bring the
    /// current match into the viewport. Returns `true` when the view's
    /// `scroll_offset` or `col_offset` changed.
    ///
    /// Deterministic and bounded: the target offset is the minimal adjustment
    /// that makes `current.buffer_row` visible. No-op when no current match
    /// or already visible.
    pub fn search_scroll_view_to_current(&self, view: &mut View) -> bool {
        self.search_state.scroll_to_current(view, &self.state)
    }

    /// Moves the live `selection` to exactly cover the current search match,
    /// if the match is currently in the live grid window; otherwise clears the
    /// live selection while keeping the search highlight (history matches are
    /// not live-selectable but remain highlight-persistent via
    /// `search_current_persistent_selection`).
    ///
    /// Returns `true` when the live selection was set to the match; `false`
    /// when the match is in history or pruned (live selection cleared).
    /// Headless: `selection_text` will then equal `matched_text` for live matches.
    pub fn search_apply_selection(&mut self) -> bool {
        let Some(pers) = self.search_state.current_persistent_selection(&self.state) else {
            return false;
        };
        // Try to restore as live-grid selection.
        if let Some(sel) = pers.to_grid_selection(&self.state) {
            self.selection = Some(sel);
            self.selection_dragging = sel.active;
            true
        } else {
            // In history or pruned: leave a history highlight but clear live selection.
            self.selection = None;
            self.selection_dragging = false;
            false
        }
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
        if timed_out {
            return false;
        }
        Self::should_proceed_after_interceptions(decisions, false)
    }

    /// Authorizes a non-local URL activation and binds it to the exact URI.
    /// The gesture must have been issued by the runtime's platform-event path;
    /// terminal output and caller-supplied booleans cannot satisfy this gate.
    pub fn authorize_url_activation(
        &mut self,
        uri: &str,
        gesture: ActivationGesture,
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> Result<UrlActivation, bitty_platform::PlatformError> {
        if self.pending_activation_gesture.as_ref() != Some(&gesture)
            || !Self::intercept_open_url(decisions, timed_out)
        {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        self.pending_activation_gesture = None;
        let validated = bitty_platform::validate_url(uri)?;
        if validated.as_str().starts_with("file:") {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        Ok(UrlActivation {
            uri: validated.as_str().to_owned(),
        })
    }

    /// Authorizes a local-file URL through a separate explicit approval path.
    pub fn authorize_file_url_activation(
        &mut self,
        uri: &str,
        gesture: ActivationGesture,
        decisions: &[InterceptionDecision],
        timed_out: bool,
    ) -> Result<FileUrlActivation, bitty_platform::PlatformError> {
        if self.pending_activation_gesture.as_ref() != Some(&gesture)
            || !Self::intercept_open_url(decisions, timed_out)
        {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        self.pending_activation_gesture = None;
        let validated = bitty_platform::validate_file_url(uri)?;
        Ok(FileUrlActivation {
            uri: validated.as_str().to_owned(),
        })
    }

    /// Takes the one-use gesture minted by a real primary mouse activation.
    /// Terminal output and synthetic API calls never mint this proof.
    pub fn take_activation_gesture(&self) -> Option<ActivationGesture> {
        self.pending_activation_gesture.clone()
    }

    /// Opens a URL using only a runtime-issued, URI-bound authorization.
    pub fn open_url(&self, activation: UrlActivation) -> Result<(), bitty_platform::PlatformError> {
        let validated = bitty_platform::validate_url(&activation.uri)?;
        if validated.as_str().starts_with("file:") {
            return Err(bitty_platform::PlatformError::UrlActivationDenied);
        }
        Self::spawn_validated_url(validated.as_str())
    }

    /// Opens a local-file URL using only its distinct runtime-issued approval.
    pub fn open_file_url(
        &self,
        activation: FileUrlActivation,
    ) -> Result<(), bitty_platform::PlatformError> {
        let validated = bitty_platform::validate_file_url(&activation.uri)?;
        Self::spawn_validated_url(validated.as_str())
    }

    fn spawn_validated_url(uri: &str) -> Result<(), bitty_platform::PlatformError> {
        use std::process::Command;
        let (program, prefix) = Self::url_dispatch();
        if cfg!(target_os = "linux") && !Self::handler_available(program) {
            return Err(bitty_platform::PlatformError::UrlLaunch(format!(
                "URL handler is unavailable: {program}"
            )));
        }
        let mut command = Command::new(program);
        if let Some(argument) = prefix {
            command.arg(argument);
        }
        command.arg(uri);
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| bitty_platform::PlatformError::UrlLaunch(error.to_string()))
    }

    fn handler_available(program: &str) -> bool {
        std::path::Path::new(program).is_file()
    }

    fn url_handler() -> &'static str {
        if cfg!(target_os = "windows") {
            r"C:\Windows\System32\explorer.exe"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "/usr/bin/gio"
        }
    }

    fn url_dispatch() -> (&'static str, Option<&'static str>) {
        if cfg!(target_os = "linux") {
            (Self::url_handler(), Some("open"))
        } else {
            (Self::url_handler(), None)
        }
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
        // CTX-0176: leaf boundaries may have moved (split/close/resize),
        // so re-sync every pane session's grid + PTY winsize to its leaf.
        self.sync_pane_geometry();
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
        // Replace any previously spawned child (drop kills old). A prior
        // wakeup forwarder (if any) is detached: it owns the old reader and
        // exits on EOF/disconnect once the old PTY is dropped.
        self.pty_forward_rx = None;
        self.pty_forward_handle = None;
        self.pty = Some(pty);
        self.pty_reader = Some(reader);
        self.pty_writer = Some(writer);
        // If a waker is already installed (respawn after `set_pty_waker`),
        // promote immediately so the new child wakes the loop too.
        if self.pty_waker.is_some() {
            self.promote_pty_reader_to_forwarder();
        }
        // Clear pending input on new shell: fresh session, no stale keystrokes.
        self.pending_input.clear();
        self.pending_input_dropped = 0;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Per-pane shell sessions (CTX-0176)
    // ------------------------------------------------------------------

    /// Spawns `program` with `args` as the private shell of layout leaf
    /// `view`, sized to `cols` x `rows` cells.
    ///
    /// Direct argv exec, no shell interpolation: the identical sandbox to
    /// [`spawn_shell_with_args`](Self::spawn_shell_with_args) — `program`
    /// plus `args` pass verbatim to the platform exec path, and a blank
    /// `program` is rejected before any spawn is attempted.
    ///
    /// The leaf must exist in the current layout; spawning for an unknown id
    /// is rejected. Re-spawning a leaf that already owns a session replaces
    /// it: the old `Pty` drops, killing and reaping its child without
    /// leaking a zombie (same lifecycle as
    /// [`spawn_shell_with_args`](Self::spawn_shell_with_args)). The session
    /// starts with a fresh grid resized to `cols` x `rows`
    /// ([`State::resize`] clamps to `1..=1000` per dimension; zero dims
    /// clamp to 1).
    ///
    /// The pane reader stays direct (no wakeup-forwarder promotion):
    /// [`poll_pty`](Self::poll_pty) drains every pane session on each call,
    /// so live panes need no waker. Pane replies flush to the pane's own
    /// writer on the same path.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `program` is blank or `view` is
    /// not a leaf of the current layout; [`RuntimeError::Pty`] when the
    /// platform reports spawn failure.
    pub fn spawn_shell_for_view(
        &mut self,
        view: ViewId,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        if !self.layout.leaf_ids().contains(&view) {
            return Err(RuntimeError::InvalidConfig(
                "view is not a leaf of the current layout",
            ));
        }
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // All fallible steps done: publish the session. A replaced session's
        // old `Pty` drops here, killing + reaping its child (no zombie).
        let mut state = State::new();
        state.resize(cols as usize, rows as usize);
        self.pane_sessions.insert(
            view,
            PaneSession {
                parser: Parser::new(),
                state,
                query_overlap: Vec::new(),
                pty,
                reader,
                writer,
            },
        );
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Whether leaf `view` owns a private shell session.
    #[must_use]
    pub fn has_pane_session(&self, view: &ViewId) -> bool {
        self.pane_sessions.contains_key(view)
    }

    /// Number of live per-pane shell sessions.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.pane_sessions.len()
    }

    /// Ids owning a private shell session, in deterministic (`ViewId`) order.
    #[must_use]
    pub fn pane_session_ids(&self) -> Vec<ViewId> {
        self.pane_sessions.keys().copied().collect()
    }

    /// Process id of the leaf's shell child, when the session exists and the
    /// platform reports one.
    #[must_use]
    pub fn pane_pid(&self, view: &ViewId) -> Option<u32> {
        self.pane_sessions.get(view).and_then(|sess| sess.pty.pid())
    }

    /// Read-only snapshot of the leaf's private grid, when it owns a
    /// session. Leaves without a session share the primary
    /// [`snapshot`](Self::snapshot).
    #[must_use]
    pub fn pane_snapshot(&self, view: &ViewId) -> Option<Snapshot> {
        self.pane_sessions
            .get(view)
            .map(|sess| sess.state.snapshot())
    }

    /// Tears down the leaf's private shell session, if any. The owned `Pty`
    /// drops, killing and reaping the child without leaking a zombie.
    /// Returns true when a session was removed.
    pub fn close_pane_session(&mut self, view: &ViewId) -> bool {
        let removed = self.pane_sessions.remove(view).is_some();
        if removed {
            self.pending_full_redraw = true;
        }
        removed
    }

    /// Re-syncs every pane session's grid + PTY winsize to its leaf's
    /// current allocation (CTX-0176). Called after layout reflows that can
    /// move leaf boundaries (`set_layout`, `reflow_to_grid`) so split panes
    /// track window resizes, DPI reflows, and split-ratio changes exactly
    /// like the primary session. Per-pane best-effort: leaves whose dims
    /// already match are skipped (keeps generations stable for
    /// frame-on-demand); a PTY resize error never fails the reflow.
    fn sync_pane_geometry(&mut self) {
        if self.pane_sessions.is_empty() {
            return;
        }
        let allocs = self.layout.layout(self.container);
        for (id, rect) in &allocs {
            let cols = rect.width.max(1);
            let rows = rect.height.max(1);
            if let Some(sess) = self.pane_sessions.get_mut(id) {
                if sess.state.width() != cols as usize || sess.state.height() != rows as usize {
                    let _ = sess.state.resize(cols as usize, rows as usize);
                    let _ = sess.pty.resize(cols, rows);
                }
            }
        }
    }

    /// Drains every pane session's direct reader into its private grid via
    /// the shared PTY pipeline (see [`handle_pane_bytes`](Self::handle_pane_bytes)),
    /// flushes pane replies to each pane's own writer, and re-syncs the
    /// global input-mode caches to the focused leaf. Returns the drained
    /// chunk count. No-op when no pane session exists.
    fn pump_pane_sessions(&mut self) -> usize {
        if self.pane_sessions.is_empty() {
            return 0;
        }
        // Collect without holding a borrow across the mutable pump calls.
        // `BTreeMap` iteration is `ViewId`-ordered, so multi-pane wakeups
        // are deterministic.
        let mut pending: Vec<(ViewId, Vec<u8>)> = Vec::new();
        for (id, sess) in self.pane_sessions.iter() {
            let mut per_pane = 0usize;
            while per_pane < 1024 {
                match sess.reader.try_recv() {
                    Some(chunk) => {
                        debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                        pending.push((*id, chunk));
                        per_pane += 1;
                    }
                    None => break,
                }
            }
        }
        let drained = pending.len();
        for (id, chunk) in pending {
            self.handle_pane_bytes(id, &chunk);
            let _ = self.write_pane_replies(id);
        }
        self.sync_mode_caches_to_focus();
        drained
    }

    /// Feeds raw PTY bytes from one pane's shell into that pane's private
    /// grid through the exact [`handle_pty_bytes`](Self::handle_pty_bytes)
    /// pipeline (query replies, clipboard policy, cold bridge, search).
    ///
    /// Implemented by swapping the pane's `Parser`/`State`/overlap tail into
    /// the primary slots for the call and swapping back afterwards, so panes
    /// get full pipeline fidelity with no duplicated logic. Unknown ids and
    /// empty input are no-ops. Single-threaded (`&mut self`), and nothing in
    /// the [`handle_pty_bytes`](Self::handle_pty_bytes) call tree touches
    /// `pane_sessions`, so the session cannot vanish mid-call; that call's
    /// documented never-panics-over-untrusted-bytes contract keeps the swap
    /// pair total.
    fn handle_pane_bytes(&mut self, view: ViewId, bytes: &[u8]) {
        if bytes.is_empty() || !self.pane_sessions.contains_key(&view) {
            return;
        }
        {
            let Some(sess) = self.pane_sessions.get_mut(&view) else {
                return;
            };
            std::mem::swap(&mut self.parser, &mut sess.parser);
            std::mem::swap(&mut self.state, &mut sess.state);
            std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
        }
        self.handle_pty_bytes(bytes);
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            // Unreachable single-threaded (see doc above); keep total rather
            // than debug-panicking on a corrupted swap pair.
            debug_assert!(false, "pane session vanished mid-pump");
            return;
        };
        std::mem::swap(&mut self.parser, &mut sess.parser);
        std::mem::swap(&mut self.state, &mut sess.state);
        std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
    }

    /// Flushes one pane's queued terminal replies to that pane's own writer
    /// (bounded, fail-closed — the per-pane mirror of
    /// [`write_replies`](Self::write_replies)). No-op without a session.
    fn write_pane_replies(&mut self, view: ViewId) -> usize {
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            return 0;
        };
        let replies = sess.state.take_replies();
        if replies.is_empty() {
            return 0;
        }
        let mut total = 0usize;
        use std::io::Write as _;
        for chunk in replies {
            // Each chunk is bounded; total bounded by the reply cap (4 KiB).
            // Best-effort, fail-closed: on write error break and drop remainder.
            if sess.writer.write_all(&chunk).is_ok() {
                total += chunk.len();
            } else {
                break;
            }
        }
        let _ = sess.writer.flush();
        total
    }

    /// Re-syncs the global Kitty/mouse-capture caches to the focused leaf's
    /// grid (or the primary grid when focus owns no session). Called after
    /// pumping panes so a mouse-tracking app in the focused pane still takes
    /// effect. No-op equivalent when no pane session exists.
    fn sync_mode_caches_to_focus(&mut self) {
        if self.pane_sessions.is_empty() {
            return;
        }
        let focused = self.focus.focused();
        let (kitty, mouse) = match focused.and_then(|id| self.pane_sessions.get(&id)) {
            Some(sess) => {
                let modes = sess.state.modes();
                (modes.kitty_keyboard, modes.mouse_tracking.is_some())
            }
            None => {
                let modes = self.state.modes();
                (modes.kitty_keyboard, modes.mouse_tracking.is_some())
            }
        };
        self.kitty_flags = kitty;
        self.mouse_capture_enabled = mouse;
    }

    /// Installs the cross-thread PTY readability callback and promotes the
    /// direct reader into the wakeup forwarder pump.
    ///
    /// The forwarder is the sole consumer of the bounded pump channel from
    /// this point: it blocks in `recv` (zero wakeups when quiet), forwards
    /// each chunk into a second bounded channel
    /// ([`PTY_FORWARD_CAPACITY_CHUNKS`]), and invokes `waker` once per chunk
    /// plus once on EOF. [`poll_pty`] drains the forwarding channel, so the
    /// existing bounded-drain contract is preserved end to end.
    ///
    /// Idempotent: replacing the waker re-promotes only when a direct reader
    /// is still present; an already-promoted pump keeps its original waker
    /// clone (all clones wake the same loop).
    pub fn set_pty_waker(&mut self, waker: PtyWaker) {
        self.pty_waker = Some(waker);
        self.promote_pty_reader_to_forwarder();
    }

    /// Whether a readability waker is installed.
    #[must_use]
    pub fn has_pty_waker(&self) -> bool {
        self.pty_waker.is_some()
    }

    /// Whether the wakeup forwarder pump is active (implies [`has_pty_reader`]).
    #[must_use]
    pub fn has_pty_forwarder(&self) -> bool {
        self.pty_forward_rx.is_some()
    }

    /// Moves the direct [`PtyReader`] into the forwarder thread when both a
    /// reader and a waker are present. No-op otherwise.
    fn promote_pty_reader_to_forwarder(&mut self) {
        if self.pty_forward_rx.is_some() {
            return;
        }
        if self.pty_reader.is_none() || self.pty_waker.is_none() {
            return;
        }
        let Some(reader) = self.pty_reader.take() else {
            return;
        };
        let Some(waker) = self.pty_waker.clone() else {
            self.pty_reader = Some(reader);
            return;
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(PTY_FORWARD_CAPACITY_CHUNKS);
        let handle = std::thread::Builder::new()
            .name("bitty-pty-wakeup".to_owned())
            .spawn(move || pty_forward_loop(reader, tx, waker))
            .expect("std thread spawn cannot fail with default builder options");
        self.pty_forward_rx = Some(rx);
        self.pty_forward_handle = Some(handle);
    }

    /// Whether a PTY child is currently owned.
    #[must_use]
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// Whether a PTY reader channel is currently owned (implies [`has_pty`]).
    ///
    /// True while either the direct pump reader or the wakeup-forwarder
    /// channel is owned.
    #[must_use]
    pub fn has_pty_reader(&self) -> bool {
        self.pty_reader.is_some() || self.pty_forward_rx.is_some()
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
    /// Only available before [`set_pty_waker`](Self::set_pty_waker) promotes
    /// the reader into the wakeup forwarder: afterwards the forwarder thread
    /// owns the reader and this returns `None` (drain via [`poll_pty`]
    /// instead). When no forwarder is active the caller becomes responsible
    /// for draining the channel without blocking the runtime thread and for
    /// joining the pump on EOF. After this call [`poll_pty`] will return `0`
    /// because the channel no longer belongs to the runtime; most embedders
    /// should prefer [`poll_pty`] instead.
    pub fn take_pty_reader(&mut self) -> Option<PtyReader> {
        if self.pty_forward_rx.is_some() {
            return None;
        }
        self.pty_reader.take()
    }

    /// Non-blocking drain of the bounded PTY output channel into
    /// [`handle_pty_bytes`].
    ///
    /// Drains the wakeup-forwarder channel when [`set_pty_waker`](Self::set_pty_waker)
    /// promoted the pump, otherwise the direct pump channel. Either way the
    /// bound holds (`CHANNEL_CAPACITY_CHUNKS` × `READ_CHUNK_SIZE` = 128 KiB
    /// per stage, 256 KiB worst-case total with the forwarder active).
    ///
    /// When a consumer stalls, the bounded channel(s) fill, the pump blocks,
    /// the kernel PTY buffer fills, and the child's writes block —
    /// end-to-end backpressure with zero data loss and zero unbounded memory
    /// growth. This method is the consumer side: it drains all immediately
    /// available chunks without blocking, feeding each through the VT parser
    /// and terminal state.
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
            if let Some(rx) = self.pty_forward_rx.as_ref() {
                let mut out = Vec::new();
                while out.len() < 1024 {
                    match rx.try_recv() {
                        Ok(chunk) => {
                            debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                            out.push(chunk);
                        }
                        Err(_) => break,
                    }
                }
                out
            } else {
                let Some(reader) = self.pty_reader.as_ref() else {
                    // No primary reader: pane sessions (if any) still pump.
                    return self.pump_pane_sessions();
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
            }
        };
        let drained = chunks.len();
        for chunk in chunks {
            self.handle_pty_bytes(&chunk);
        }
        // Bounded PTY reply loop: parse->state->replies->writer (4 KiB cap, fail-closed)
        // Headless (no writer) keeps replies queued for `take_replies` observation.
        let _ = self.write_replies();
        // CTX-0176: pump every pane session on the same bounded path.
        drained + self.pump_pane_sessions()
    }

    /// Blocking drain with a timeout, returning the number of chunks drained.
    ///
    /// Blocks at most `timeout` for the first chunk; once data is flowing it
    /// drains all immediately available chunks without further blocking. Useful
    /// for tests that need to wait for a shell echo. Returns `0` on timeout
    /// or EOF.
    pub fn poll_pty_timeout(&mut self, timeout: std::time::Duration) -> usize {
        let first: Option<Vec<u8>> = {
            if let Some(rx) = self.pty_forward_rx.as_ref() {
                rx.recv_timeout(timeout).ok()
            } else {
                let Some(reader) = self.pty_reader.as_ref() else {
                    return 0;
                };
                match reader.recv_timeout(timeout) {
                    Ok(Some(chunk)) => Some(chunk),
                    Ok(None) | Err(_) => None,
                }
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
        // CTX-0146: pre-scan overlap ++ new bytes for parameterized queries
        // (DECRQM mode numbers, XTGETTCAP payloads, secondary-DA request
        // forms). Bounded scans; matches ending inside the overlap were
        // answered on the earlier call and are filtered by the scanners.
        let overlap_len = self.query_overlap.len();
        let mut combined = Vec::with_capacity(overlap_len + bytes.len());
        combined.extend_from_slice(&self.query_overlap);
        combined.extend_from_slice(bytes);
        let mut decrqm = crate::queries::find_decrqm(&combined, overlap_len);
        let mut secondary = crate::queries::find_secondary_da(&combined, overlap_len);
        let mut tcaps = crate::queries::find_xtgettcap(&combined, overlap_len);
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
                TerminalAction::OscPromptMark { kind, .. } => Some(ColdEvent::ZoneMarked(*kind)),
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
            // OSC 52 clipboard (P0-AC-007): separate read/write policy.
            // Writes are capability-gated (clipboard.write); reads are consent-gated (clipboard.read).
            // Both default to deny. Untrusted PTY bytes cannot trigger clipboard
            // I/O without the corresponding granted capability / consent flag.
            if let TerminalAction::OscClipboard { op, data } = &action {
                match op {
                    ClipboardOp::Write => {
                        if !self.osc_clipboard_write_allowed {
                            continue;
                        }
                        let raw = String::from_utf8_lossy(data.as_bytes()).into_owned();
                        self.clipboard.set_text_lossy(raw);
                    }
                    ClipboardOp::Read => {
                        // Denied without explicit read consent (P0-AC-007):
                        // no data leaves the clipboard, no reply queued.
                        if !self.osc_clipboard_read_allowed {
                            continue;
                        }
                        // Even when allowed, read consent must be explicit:
                        // synthesize a read reply only when the caller has set
                        // `osc_clipboard_read_allowed = true` via policy gate.
                        // headless path: place clipboard text into reply queue
                        // bounded via State reply cap; here we push via clipboard read.
                        let _ = self.clipboard.get_text();
                    }
                }
            }
            let damage = self.state.apply(&action);
            // Keep input-related mode caches in sync (Kitty, mouse capture)
            self.kitty_flags = self.state.modes().kitty_keyboard;
            self.mouse_capture_enabled = self.state.modes().mouse_tracking.is_some();
            if !damage.regions.is_empty() {
                let generation = damage.generation;
                self.cold_queue.push(ColdEvent::Damage { generation });
                // Bridge damage generation as well.
                self.plugin_host
                    .push_observation(HostObservation::Damage { generation });
            }
            // Selection persistence (CTX-0060): FullReset erases grid and scrollback,
            // so any live selection is no longer anchored to valid content.
            // ED 3 (EraseDisplayMode::Scrollback) clears scrollback history but
            // leaves the live grid; live-grid selections remain valid. We only
            // clear on FullReset here; scrollback-only clears keep live selection.
            if matches!(action, TerminalAction::FullReset) {
                self.clear_selection();
            }
            // CTX-0146 (Issue #238): answer standard terminal queries with
            // true capabilities. The parser maps these shapes to `Unknown`
            // (inert for the grid); the runtime queues the bounded reply via
            // the existing `Reply` action so the 4 KiB reply cap and the
            // `poll_pty -> write_replies` flush path apply unchanged.
            // Parameterized families additionally require their raw match
            // (pre-scanned above), so bytes buried inside OSC strings can
            // never spoof a reply, and stale overlap matches (answered on an
            // earlier call) are never re-answered.
            if let TerminalAction::Unknown(seq) = &action {
                let mut pending: Vec<Vec<u8>> = Vec::new();
                if crate::queries::is_secondary_da(seq) {
                    if secondary > 0 {
                        secondary -= 1;
                        pending.push(crate::queries::secondary_da_reply());
                    }
                } else if crate::queries::is_xterm_version(seq) {
                    pending.push(crate::queries::xterm_version_reply());
                } else if crate::queries::is_legacy_decid(seq) {
                    pending.push(crate::queries::primary_da_reply());
                } else if crate::queries::is_decrqm_private(seq)
                    || crate::queries::is_decrqm_ansi(seq)
                {
                    let want_private = crate::queries::is_decrqm_private(seq);
                    if let Some(pos) = decrqm.iter().position(|m| m.private == want_private) {
                        let query = decrqm.remove(pos);
                        for mode in query.modes {
                            let value =
                                crate::queries::decrqm_value(&self.state, want_private, mode);
                            pending.push(crate::queries::decrpm_reply(want_private, mode, value));
                        }
                    }
                } else if crate::queries::is_xtgettcap(seq) && !tcaps.is_empty() {
                    let query = tcaps.remove(0);
                    pending.push(crate::queries::xtgettcap_reply(&query.payload));
                }
                for reply in pending {
                    self.state.apply(&TerminalAction::Reply {
                        bytes: reply.into_boxed_slice(),
                    });
                }
            }
        }
        // Retain a bounded tail so a query split over two PTY reads is still
        // recognized on the next call. Capacity stays near the overlap bound:
        // huge chunks shrink back instead of pinning a large buffer.
        self.query_overlap.extend_from_slice(bytes);
        if self.query_overlap.len() > crate::queries::QUERY_OVERLAP_MAX {
            let excess = self.query_overlap.len() - crate::queries::QUERY_OVERLAP_MAX;
            self.query_overlap.drain(..excess);
        }
        if self.query_overlap.capacity() > crate::queries::QUERY_OVERLAP_MAX * 2 {
            self.query_overlap
                .shrink_to(crate::queries::QUERY_OVERLAP_MAX * 2);
        }
        // Search UI integration (CTX-0061): keep bounded matches in sync after
        // state growth/scrollback pushes; headless refresh is cheap (truncated
        // pattern, capped results) and deterministic. No I/O.
        if self.search_state.is_active() {
            self.search_state.refresh(&self.state);
        }
    }

    /// Handles a physical-pixel resize: recomputes the grid size from the
    /// live (possibly DPI-scaled) cell metrics, reconfigures the software
    /// surface, updates the layout container, reflows leaf views, resizes
    /// the terminal grid via the singular reflow (truncate/pad with orphan
    /// repair) and resizes the PTY when present. Zero-sized extents are
    /// skipped (minimized/occluded windows) per the
    /// `map_resize_to_surface_extent` contract. The reflow is deterministic
    /// and headless-testable.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Render`] when headless reconfiguration rejects the
    /// extent; [`RuntimeError::Pty`] when the PTY resize fails.
    pub fn handle_resize(&mut self, size: PhysicalSize) -> Result<(), RuntimeError> {
        if bitty_platform::map_resize_to_surface_extent(size).is_none() {
            return Ok(());
        }
        let (new_cols, new_rows) = self.grid_from_physical(size);
        self.reflow_to_grid(new_cols, new_rows, size)
    }

    /// Base (design) cell metrics from the validated config.
    ///
    /// The renderer starts here at scale 1.0; [`Self::apply_dpi_scale`]
    /// derives scaled cells from this base on every change so repeated scale
    /// changes never compound rounding (each rescale starts from the design
    /// cell instead of re-scaling the previous scaled cell).
    fn base_cell_metrics(&self) -> CellMetrics {
        CellMetrics::new(self.config.cell_width, self.config.cell_height)
            .expect("validated config guarantees non-zero cell metrics")
    }

    /// Base font query from the validated config.
    fn base_font_query(&self) -> FontQuery {
        FontQuery {
            family: self.config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: self.config.font_size,
        }
    }

    /// Currently live cell metrics: the design cell at scale 1.0, the
    /// DPI-scaled cell after [`Self::apply_dpi_scale`].
    ///
    /// All physical-pixel geometry (grid derivation, cursor mapping,
    /// per-leaf pixel origins) flows through this so the renderer placement
    /// and the runtime geometry can never disagree about the cell size.
    fn live_cell_metrics(&self) -> CellMetrics {
        self.renderer.cell_metrics()
    }

    /// Last adopted DPI scale (1.0 until [`Self::apply_dpi_scale`] or a
    /// `ScaleFactorChanged` event adopts another; always sanitized, so this
    /// never reports zero, negative, NaN, or infinite).
    #[must_use]
    pub fn dpi_scale(&self) -> f64 {
        self.scale_factor.get()
    }

    /// Derives `cols`/`rows` from a physical extent over the live (possibly
    /// DPI-scaled) cell metrics, saturating to at least 1x1 and capping at
    /// the 1000x1000 grid bound so hostile extents cannot grow grid memory
    /// without limit (mirrors [`RuntimeConfig::grid_from_pixels`]).
    fn grid_from_physical(&self, extent: PhysicalSize) -> (usize, usize) {
        let (cols, rows) = grid_from_surface_extent(extent, self.live_cell_metrics());
        (cols.clamp(1, 1000), rows.clamp(1, 1000))
    }

    /// Pixel extent the combined frame's plan covers: container cells at the
    /// live (DPI-scaled) cell metrics.
    ///
    /// The GPU present path recovers its per-frame NDC factor as
    /// `surface / plan` ([`bitty_render::batch::derive_scale`]), so this
    /// must describe the DrawList's own pixel space — not the 1x1 probe the
    /// combined list is synthesized from (a stale 1-cell extent clamps the
    /// factor to 4x and magnifies the whole frame, the dominant blur behind
    /// #232 alongside the unscaled surface/grid/atlas).
    fn present_plan_extent(&self) -> bitty_render::geometry::ExtentPx {
        self.live_cell_metrics().extent_for(
            usize::from(self.container.width),
            usize::from(self.container.height),
        )
    }

    /// Adopts a DPI scale change (fail-safe, headless-testable, no I/O).
    ///
    /// Sanitizes `scale` via [`sanitize_dpi_scale`] (invalid input becomes
    /// 1.0, hostile input clamps to `[MIN_DPI_SCALE, MAX_DPI_SCALE]` — never
    /// panics), reloads the renderer's font at the scaled size through
    /// `GridRenderer::apply_dpi_scale`, and — when `physical_extent` carries
    /// a non-zero extent — derives the grid from physical pixels over the
    /// scaled cells ([`grid_from_surface_extent`]) and reflows state, layout,
    /// surface, and PTY to match.
    ///
    /// Callers holding only cached logical geometry should convert it first
    /// via [`bitty_platform::surface_extent_from_logical`] and pass the
    /// result here; callers with a live window must prefer re-reading the
    /// physical `inner_size` (already physical pixels — the winit logical
    /// size path is the suspected original sin behind #232). A following
    /// `Resized` event takes precedence either way: [`Self::handle_resize`]
    /// derives from the same live scaled cells.
    ///
    /// Passing `None` rescales the renderer and forces a full redraw while
    /// leaving the grid for the next `Resized` (this is what the
    /// `ScaleFactorChanged` event path does when no window handle is at
    /// hand).
    ///
    /// Never strands the window: renderer font-load failures keep the
    /// previous cells/grid and still force a full redraw; reflow failures
    /// after a successful rescale are absorbed for the same reason.
    pub fn apply_dpi_scale(&mut self, scale: f64, physical_extent: Option<PhysicalSize>) {
        let sanitized = sanitize_dpi_scale(scale);
        let base_cell = self.base_cell_metrics();
        let base_query = self.base_font_query();
        if self
            .renderer
            .apply_dpi_scale(base_cell, &base_query, sanitized)
            .is_err()
        {
            // Fail-safe: keep previous cells/grid; the window stays drawable.
            self.pending_full_redraw = true;
            return;
        }
        self.scale_factor = ScaleFactor::new_sanitized(sanitized);
        if let Some(extent) = physical_extent.and_then(bitty_platform::map_resize_to_surface_extent)
        {
            let (cols, rows) = self.grid_from_physical(extent);
            // Absorb reflow errors after a successful rescale so the adopted
            // renderer cells always win over stranding the window.
            let _ = self.reflow_to_grid(cols, rows, extent);
        }
        self.pending_full_redraw = true;
    }

    /// Reflows terminal state, layout, surface, and PTY to `cols`/`rows`
    /// with `surface_extent` as the configured extent. Shared by
    /// [`Self::handle_resize`] and [`Self::apply_dpi_scale`] so both paths
    /// stay consistent.
    fn reflow_to_grid(
        &mut self,
        cols: usize,
        rows: usize,
        surface_extent: PhysicalSize,
    ) -> Result<(), RuntimeError> {
        // Resize terminal state first so snapshot dimensions reflect the new
        // geometry before layout and surface work; resize also emits full
        // damage (grid + scrollback reflow) with a new generation.
        let _damage = self.state.resize(cols, rows);
        self.cols = cols;
        self.rows = rows;
        self.container = default_container(cols, rows);
        // Clamp any leaf View scroll offsets to the new scrollback limit
        // (scrollback may have been truncated on shrink, though we preserve
        // ids; clamp keeps offset in-bounds deterministically).
        let max_scrollback = self.state.scrollback_len();
        let ids = self.layout.leaf_ids();
        for id in ids {
            if let Some(view) = self.layout.find_leaf_mut(id) {
                view.clamp_scroll_offset(max_scrollback);
            }
        }
        self.layout.reflow(self.container);
        // CTX-0176: the container moved, so every pane session's grid +
        // PTY winsize follows its leaf (primary state/PTY handled below).
        self.sync_pane_geometry();
        // Clamp selection to new snapshot bounds (keeps invariants after reflow;
        // wide-char snapping is preserved). Headless so deterministic.
        if let Some(sel) = self.selection {
            let snap = self.state.snapshot();
            let clamped = sel.clamped(&snap).snapped(Some(&snap));
            if clamped.is_empty() {
                self.selection = None;
                self.selection_dragging = false;
            } else {
                self.selection = Some(clamped);
            }
        }
        // Search UI integration (CTX-0061): clamp matches to new geometry; refresh
        // is bounded and deterministic. Keeps current index clamped.
        if self.search_state.is_active() {
            self.search_state.refresh(&self.state);
        }
        // Surface resize: real GPU path when attached, else headless
        if let Some(gpu) = self.gpu.as_ref() {
            self.surface
                .resize(gpu, surface_extent)
                .map_err(RuntimeError::from)?;
        } else {
            self.surface
                .headless_resize(surface_extent)
                .map_err(RuntimeError::from)?;
        }
        if let Some(pty) = self.pty.as_mut() {
            let pty_cols = self.cols.min(u16::MAX as usize) as u16;
            let pty_rows = self.rows.min(u16::MAX as usize) as u16;
            pty.resize(pty_cols, pty_rows).map_err(RuntimeError::from)?;
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Handles one platform event, returning `true` when the event asks the
    /// application loop to exit (window close requested or `Exiting` phase).
    ///
    /// Resize events are routed through [`Self::handle_resize`]; keyboard
    /// input is encoded via the legacy xterm table plus Kitty opt-in (7727) and
    /// routed to the PTY writer when live, otherwise buffered as bounded pending input
    /// for headless observation. Mouse and cursor events drive the
    /// headless-tested selection state via `bitty-ui::Selection`
    /// with wide-char snapping, or when mouse 1000/1002/1003 + 1006 SGR capture active
    /// encode to bounded SGR bytes (≤32 per event). Focus 1004 emits CSI I/O,
    /// bracketed paste 2004 wraps commits, wheel accumulates pixel deltas to cell lines,
    /// and IME preedit is presentation overlay.
    pub fn handle_platform_event(&mut self, event: PlatformEvent) -> bool {
        match event {
            PlatformEvent::Window { window_id: _, kind } => match kind {
                WindowEventKind::Resized(size) => {
                    let _ = self.handle_resize(size);
                    false
                }
                WindowEventKind::ScaleFactorChanged(factor) => {
                    // Adopt immediately (fail-safe: sanitized, never panics,
                    // never strands the window). The grid follows when the
                    // embedder re-reads the physical inner_size (see
                    // apply_dpi_scale) or from the next Resized, which takes
                    // precedence either way.
                    self.apply_dpi_scale(factor.get(), None);
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
                WindowEventKind::MouseInput(mouse) => {
                    self.handle_mouse_input(mouse);
                    if mouse.button == MouseButton::Left && mouse.state == PressState::Released {
                        if let Some(pos) = self.last_cursor {
                            let cell = self.cursor_to_cell(pos);
                            let snapshot = self.state.snapshot();
                            let Some(index) = (cell.row as usize)
                                .checked_mul(snapshot.width)
                                .and_then(|base| base.checked_add(cell.col as usize))
                            else {
                                return false;
                            };
                            if let Some(id) = snapshot.cells.get(index).and_then(|c| c.hyperlink) {
                                if let Some((_, uri)) = self.state.hyperlink_entry(id) {
                                    let is_safe = if uri.starts_with("file:") {
                                        bitty_platform::validate_file_url(uri).is_ok()
                                    } else {
                                        bitty_platform::validate_url(uri).is_ok()
                                    };
                                    if is_safe {
                                        let token = ActivationGesture(self.next_activation_gesture);
                                        self.next_activation_gesture =
                                            self.next_activation_gesture.wrapping_add(1).max(1);
                                        self.pending_activation_gesture = Some(token);
                                    }
                                }
                            }
                        }
                    }
                    false
                }
                WindowEventKind::CursorMoved(pos) => {
                    self.handle_cursor_moved(pos);
                    false
                }
                WindowEventKind::CursorLeft => {
                    // Cursor left window: end drag if active (deterministic).
                    if self.selection_dragging {
                        self.selection_dragging = false;
                        if let Some(mut sel) = self.selection {
                            sel.active = false;
                            self.selection = Some(sel);
                        }
                    }
                    false
                }
                WindowEventKind::MouseWheel(delta) => {
                    self.handle_wheel(delta);
                    false
                }
                WindowEventKind::Focused(focused) => {
                    self.set_focused(focused);
                    false
                }
                WindowEventKind::ModifiersChanged(mods) => {
                    self.shift_pressed = mods.shift;
                    self.control_pressed = mods.control;
                    self.alt_pressed = mods.alt;
                    // CTX-0159: retain modifier latch changes for probes.
                    self.inspect_ring.push_modifiers(
                        self.shift_pressed,
                        self.control_pressed,
                        self.alt_pressed,
                    );
                    self.publish_inspect_snapshot();
                    false
                }
                WindowEventKind::Ime(ime) => {
                    match ime {
                        bitty_platform::ImeEvent::Preedit(text, cursor) => {
                            if text.is_empty() {
                                self.handle_ime_preedit(None, None);
                            } else {
                                let cur = cursor.map(|(s, _)| s);
                                self.handle_ime_preedit(Some(text), cur);
                            }
                        }
                        bitty_platform::ImeEvent::Commit(text) => {
                            self.handle_ime_commit(text);
                        }
                        bitty_platform::ImeEvent::Enabled | bitty_platform::ImeEvent::Disabled => {
                            // No grid mutation; just ensure overlay cleared on disabled
                            if matches!(ime, bitty_platform::ImeEvent::Disabled) {
                                self.handle_ime_preedit(None, None);
                            }
                        }
                    }
                    false
                }
            },
            PlatformEvent::Exiting => true,
            PlatformEvent::Resumed => {
                self.pending_full_redraw = true;
                false
            }
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

    /// Writes pending PTY replies to the PTY writer (bounded, fail-closed).
    ///
    /// Forms the `poll_pty()->parse->state->replies->bounded PtyWriter::write_all()`
    /// loop for DSR/DA/cursor queries (DSR 6, DA `CSI c`, etc.). Bounded by
    /// `REPLY_CAP_BYTES` (4 KiB) in `State` (DropNewest per RFC invariant 7
    /// and counted via `replies_overflowed`). The hot path (`handle_pty_bytes`
    /// parsing and state apply) never blocks; this method is the only
    /// producer into the PTY master for replies and is best-effort, never
    /// panics, never interpolates through a shell, and never grows without
    /// bound. No shell interpolation, no unbounded buffering.
    ///
    /// When no live `PtyWriter` is owned (headless CI), the replies remain
    /// queued for `take_replies` observation and no I/O is performed (0
    /// returned). When a writer is present the replies are drained and each
    /// chunk is `write_all` + `flush` best-effort; errors are ignored
    /// (fail-closed, reply dropped, overflow already counted). Returns the
    /// total bytes successfully written (≤ 4 KiB, bounded).
    pub fn write_replies(&mut self) -> usize {
        if self.pty_writer.is_none() {
            return 0;
        }
        let replies = self.state.take_replies();
        if replies.is_empty() {
            return 0;
        }
        let Some(writer) = self.pty_writer.as_mut() else {
            return 0;
        };
        let mut total = 0usize;
        use std::io::Write as _;
        for chunk in replies {
            // Each chunk is bounded; total bounded by REPLY_CAP_BYTES (4 KiB).
            // Best-effort, fail-closed: on write error break and drop remainder.
            if writer.write_all(&chunk).is_ok() {
                total += chunk.len();
            } else {
                break;
            }
        }
        let _ = writer.flush();
        total
    }

    /// Alias for `write_replies` for embedders that use the `flush_pty_replies`
    /// name (both drain through the same bounded, fail-closed writer path).
    pub fn flush_pty_replies(&mut self) -> usize {
        self.write_replies()
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
        self.tick_at(std::time::Instant::now())
    }

    /// Tick with an explicit wall clock (CTX-0192 virtual-clock seam).
    ///
    /// `tick()` delegates with `Instant::now()`; tests pass virtual times to
    /// prove the transient banner: full summary → flash after
    /// [`PASTE_BANNER_FULL_DURATION`] → still pending (never-silent) → gone
    /// on confirm/cancel. Behavior is otherwise identical to `tick()`.
    pub fn tick_at(&mut self, now: std::time::Instant) -> Option<PresentStats> {
        // CTX-0192 transient: collapse the full banner to the flash once its
        // duration expires. Force exactly one repaint for the transition so
        // the retained frame keeps a visible (smaller) signal while pending.
        if self.pending_paste.is_some() {
            match self.pending_paste_since {
                Some(since) => {
                    let collapsed =
                        now.saturating_duration_since(since) >= PASTE_BANNER_FULL_DURATION;
                    if collapsed != self.paste_banner_collapsed {
                        self.paste_banner_collapsed = collapsed;
                        self.pending_full_redraw = true;
                    }
                }
                None => {
                    self.pending_paste_since = Some(now);
                    self.paste_banner_collapsed = false;
                    self.pending_full_redraw = true;
                }
            }
        }
        // Reflow layout tree into container before rendering so leaf Views
        // carry deterministic origins/sizes for this frame. This is headless
        // and deterministic: same layout + container always yields same
        // allocations.
        self.layout.reflow(self.container);

        let snapshot = self.state.snapshot();
        let pending_full = self.pending_full_redraw;
        let last = self.last_presented_generation;
        // CTX-0176: the presented generation tracks the newest grid across
        // the primary state and every pane session, so frame-on-demand idles
        // only when all shells are quiet.
        let mut current_gen = snapshot.generation;
        for sess in self.pane_sessions.values() {
            current_gen = current_gen.max(sess.state.generation());
        }

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
        // CTX-0176: per-pane grids carry independent generations that the
        // shared damage ring cannot see, so any live pane session forces the
        // full per-leaf path (over-damage safe, deterministic).
        let use_full = !self.pane_sessions.is_empty()
            || pending_full
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

        // For single-window slice we need per-view scrollback viewport and cursor.
        // Build id->View map for scroll/IME lookups.
        let view_map: std::collections::HashMap<ViewId, View> = {
            let mut m = std::collections::HashMap::new();
            for (vid, r) in &allocations {
                if let Some(v) = self.layout.find_leaf(*vid) {
                    m.insert(*vid, v.clone());
                } else {
                    // Fallback: synthesized view sized to allocation
                    let mut v = View::new(*vid, r.width as usize, r.height as usize);
                    v.set_origin(bitty_ui::Point::new(r.x, r.y));
                    m.insert(*vid, v);
                }
            }
            m
        };

        for (view_id, rect) in &allocations {
            if rect.is_empty() {
                continue;
            }
            // CTX-0176: a leaf with its own shell renders that session's
            // grid; leaves without a session share the primary snapshot
            // (the unchanged single-pane path while no session exists).
            let pane_snap: Option<Snapshot> = if self.pane_sessions.is_empty() {
                None
            } else {
                Some(match self.pane_sessions.get(view_id) {
                    Some(sess) => sess.state.snapshot(),
                    None => snapshot.clone(),
                })
            };
            let base_snap: &Snapshot = pane_snap.as_ref().unwrap_or(&snapshot);
            // Determine viewport snapshot: when view scroll_offset !=0, visible_cells composites scrollback.
            let view = view_map.get(view_id);
            let view_snapshot = if let Some(v) = view {
                if v.scroll_offset() != 0 {
                    let cells = match self.pane_sessions.get(view_id) {
                        Some(sess) => v.visible_cells(&sess.state),
                        None => v.visible_cells(&self.state),
                    };
                    Snapshot {
                        version: base_snap.version,
                        generation: base_snap.generation,
                        width: v.cols() as usize,
                        height: v.rows() as usize,
                        cells,
                        cursor: base_snap.cursor.clone(),
                        modes: base_snap.modes.clone(),
                        title: base_snap.title.clone(),
                    }
                } else {
                    viewport_snapshot(base_snap, rect.width, rect.height)
                }
            } else {
                viewport_snapshot(base_snap, rect.width, rect.height)
            };

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
            // Cursor rendering: add a cursor fill when visible, focused, and live.
            // Single-window slice: cursor is presentation overlay, not terminal truth mutation.
            let mut list = list;
            if view_snapshot.cursor.visible
                && self.focused
                && view.map(|v| v.scroll_offset() == 0).unwrap_or(true)
            {
                // Only draw cursor when this view is the focused view (or single leaf default)
                let is_focused_view = self
                    .focused_view()
                    .map(|fid| fid == *view_id)
                    .unwrap_or(true);
                if is_focused_view {
                    let cur = &view_snapshot.cursor.position;
                    if (cur.row as usize) < view_snapshot.height
                        && (cur.col as usize) < view_snapshot.width
                    {
                        // Check not on spacer
                        let idx = cur.row as usize * view_snapshot.width + cur.col as usize;
                        let is_spacer = view_snapshot
                            .cells
                            .get(idx)
                            .map(|c| c.spacer)
                            .unwrap_or(false);
                        if !is_spacer {
                            let live = self.live_cell_metrics();
                            // DECSCUSR shape comes from the shared render primitive
                            // (`bitty_render::grid::cursor_fill`: block = full cell,
                            // bar = left strip, underline = bottom strip, 15% thickness
                            // per DEC-0017 ghostty/alacritty refs). Geometry is shared;
                            // the overlay hue stays the slice's translucent white.
                            if let Some(fill) = bitty_render::grid::cursor_fill(
                                &view_snapshot.cursor,
                                live,
                                view_snapshot.width,
                                view_snapshot.height,
                            ) {
                                // Cursor color: inverse of cell bg/fg? Use resolved color from grid.rs DEFAULT_FG/BG inversion.
                                // For slice, use white with 0x80 alpha for cursor, respecting blinking flag;
                                // if blinking and not focused, we skip (already checked focused).
                                let cursor_color: bitty_render::grid::Rgba8 =
                                    if view_snapshot.cursor.visible {
                                        [0xFF, 0xFF, 0xFF, 0xA0]
                                    } else {
                                        [0, 0, 0, 0]
                                    };
                                list.fills.push(bitty_render::grid::FillRect {
                                    rect: fill.rect,
                                    color: cursor_color,
                                });
                            }
                        }
                    }
                }
            }

            if !list.needs_draw() {
                continue;
            }
            any_needs_draw = true;

            let live = self.live_cell_metrics();
            let origin_px_x = rect.x as i32 * live.width as i32;
            let origin_px_y = rect.y as i32 * live.height as i32;
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

        // Selection highlight overlay (CTX-0158, ghostty selection rendering):
        // presentation-only fills in the theme selection color, painted above
        // cell backgrounds. `DrawList` paint order is fills first, then
        // glyphs, so the highlight tints the background while text stays
        // legible on top. Bounded: at most one rect per selected row.
        // Skipped while the focused view is scrolled into history (the live
        // grid selection does not map to the scrollback viewport).
        if let Some(sel) = self.selection {
            if !sel.is_empty() {
                let norm = sel.normalized();
                let scrolled = self
                    .focused_view()
                    .and_then(|fid| view_map.get(&fid))
                    .map(|v| v.scroll_offset() != 0)
                    .unwrap_or(false);
                if !scrolled {
                    let live = self.live_cell_metrics();
                    let fid = self.focused_view().or(view_map.keys().next().copied());
                    if let Some(focused_id) = fid {
                        if let Some((_, rect)) =
                            allocations.iter().find(|(id, _)| *id == focused_id)
                        {
                            let rects = bitty_render::grid::selection_fill_rects(
                                (norm.start.row, norm.start.col),
                                (norm.end.row, norm.end.col),
                                snapshot.width,
                                snapshot.height,
                                live,
                            );
                            if !rects.is_empty() {
                                let origin_px_x = rect.x as i32 * live.width as i32;
                                let origin_px_y = rect.y as i32 * live.height as i32;
                                for mut fill in rects {
                                    fill.rect.x += origin_px_x;
                                    fill.rect.y += origin_px_y;
                                    combined_fills.push(fill);
                                }
                                any_needs_draw = true;
                            }
                        }
                    }
                }
            }
        }

        // IME preedit overlay: presentation-only, not state mutation. Paints atop cursor.
        if let Some(preedit) = self.ime_preedit.clone() {
            if !preedit.is_empty() && self.focused {
                // Determine focused view allocation origin and cursor pixel position.
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some((vid, rect)) = allocations.iter().find(|(id, _)| *id == fid) {
                        let live = self.live_cell_metrics();
                        // CTX-0176: the preedit overlay tracks the focused
                        // leaf's cursor, so IME lands on the pane receiving
                        // input (primary grid when focus owns no session).
                        let cur = self
                            .focused_view()
                            .and_then(|focused| self.pane_sessions.get(&focused))
                            .map(|sess| sess.state.snapshot().cursor.position)
                            .unwrap_or(snapshot.cursor.position);
                        let origin_px_x = rect.x as i32 * live.width as i32;
                        let origin_px_y = rect.y as i32 * live.height as i32;
                        let base_x = origin_px_x + cur.col as i32 * live.width as i32;
                        let base_y = origin_px_y + cur.row as i32 * live.height as i32;
                        // Simple IME overlay: underline background rect plus glyphs for preedit chars.
                        // For slice, render preedit as single underline fill plus per-char glyphs via renderer? Simplified: add a fill rect for underline.
                        let preedit_width = (preedit.chars().count() as u32 * live.width).min(1024);
                        let underline_rect = bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y + live.height as i32 - 2,
                            preedit_width,
                            2,
                        );
                        combined_fills.push(bitty_render::grid::FillRect {
                            rect: underline_rect,
                            color: [0xFF, 0xFF, 0x00, 0xFF],
                        });
                        // Also push a background fill for preedit area (semi-transparent)
                        let bg_rect = bitty_render::geometry::RectPx::new(
                            base_x,
                            base_y,
                            preedit_width,
                            live.height,
                        );
                        combined_fills.push(bitty_render::grid::FillRect {
                            rect: bg_rect,
                            color: [0x33, 0x33, 0x33, 0xCC],
                        });
                        any_needs_draw = true;
                        let _ = vid; // keep
                    }
                }
            }
        }

        // Pending-paste confirmation banner (CTX-0186, transient CTX-0192):
        // presentation-only overlay on the focused view's bottom row,
        // right-aligned compact pill (not full-width) to avoid occluding the
        // grid. Gated on `has_pending_paste()`; text is the bounded compact
        // `pending_paste_summary()` for `PASTE_BANNER_FULL_DURATION`, then the
        // minimal `PASTE_BANNER_FLASH_TEXT` while pending (never-silent).
        // Overlay only: pushes fills+glyphs onto the combined frame, never
        // touches grid cells, scrollback, or the pending bytes. Esc-cancel
        // and repeat-confirm paths are unchanged; clearing pending repaints
        // once without the banner via `pending_full_redraw`.
        if self.has_pending_paste() {
            if let Some(banner) = self.paste_banner_text_at(now) {
                if let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) {
                    if let Some((_, rect)) = allocations.iter().find(|(id, _)| *id == fid) {
                        if rect.height > 0 && rect.width > 0 {
                            let live = self.live_cell_metrics();
                            let max_cells = rect.width as usize;
                            // Compact pill: only as wide as the text (clipped
                            // to the view), right-aligned so most of the row
                            // stays visible.
                            let text_cells = banner.chars().count().min(max_cells).max(1);
                            let pill_w = text_cells as u32 * live.width;
                            let full_w = rect.width as u32 * live.width;
                            let origin_px_x = rect.x as i32 * live.width as i32
                                + (full_w.saturating_sub(pill_w)) as i32;
                            let banner_y =
                                (rect.y as i32 + rect.height as i32 - 1) * (live.height as i32);
                            combined_fills.push(bitty_render::grid::FillRect {
                                rect: bitty_render::geometry::RectPx::new(
                                    origin_px_x,
                                    banner_y,
                                    pill_w,
                                    live.height,
                                ),
                                color: bitty_render::grid::PENDING_PASTE_BANNER_BG,
                            });
                            let glyphs = self.renderer.overlay_text_glyphs(
                                &banner,
                                (origin_px_x, banner_y),
                                max_cells,
                                bitty_render::grid::PENDING_PASTE_BANNER_FG,
                            );
                            combined_glyphs.extend(glyphs);
                            any_needs_draw = true;
                        }
                    }
                }
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
            // Now replace fills/glyphs with combined, and reset the plan to
            // describe the combined pixel space: the 1x1 probe's extent must
            // not survive (see present_plan_extent), and the dirty rect must
            // cover the combined frame when content exists.
            tmp_list.generation = current_gen;
            tmp_list.fills = combined_fills;
            tmp_list.glyphs = combined_glyphs;
            let plan_extent = self.present_plan_extent();
            tmp_list.plan.extent = plan_extent;
            if tmp_list.fills.is_empty() && tmp_list.glyphs.is_empty() {
                tmp_list.plan.dirty_rects = Vec::new();
            } else {
                tmp_list.plan.dirty_rects = vec![bitty_render::geometry::RectPx::new(
                    0,
                    0,
                    plan_extent.width,
                    plan_extent.height,
                )];
                tmp_list.plan.mode = FrameMode::Full;
            }
            tmp_list
        };

        if !combined_list.needs_draw() {
            self.last_presented_generation = current_gen;
            return None;
        }

        let atlas_texels = self.renderer.atlas_texels().to_vec();
        let dims = self.renderer.atlas_dims();
        let stats = if let Some(gpu) = self.gpu.as_ref() {
            match self
                .surface
                .present_draw_list(gpu, &combined_list, Some((&atlas_texels, dims)))
            {
                Ok(s) => s,
                Err(_) => {
                    // GPU present failed (surface lost/outdated): fallback to headless for this frame
                    match self
                        .surface
                        .headless_present(&combined_list, Some((&atlas_texels, dims)))
                    {
                        Ok(h) => h,
                        Err(_) => return None,
                    }
                }
            }
        } else {
            match self
                .surface
                .headless_present(&combined_list, Some((&atlas_texels, dims)))
            {
                Ok(stats) => stats,
                Err(_) => return None,
            }
        };
        self.last_presented_generation = current_gen;
        // CTX-0159: publish grid plus latched input/focus so socket probes see
        // typed text without screenshots (`&self` only, bounded).
        self.publish_inspect_snapshot();
        Some(PresentStats {
            frame: stats.frame,
            fills: stats.fills,
            glyphs: stats.glyphs,
            headless: stats.headless,
            generation: current_gen,
        })
    }
}

fn truncate_paste_text(text: String) -> String {
    const MAX_BYTES: usize = bitty_platform::clipboard::CLIPBOARD_MAX_BYTES;
    if text.len() <= MAX_BYTES {
        return text;
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Truncate `s` to at most `max_bytes` at a char boundary (CTX-0186 summary).
fn truncate_str_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ui::{Rect as UiRect, SplitAxis};

    fn make_runtime() -> Runtime {
        Runtime::with_defaults().expect("defaults must build")
    }

    fn test_char_key(logical: &str, text: Option<&str>, state: PressState) -> KeyEvent {
        KeyEvent {
            logical_key: bitty_platform::LogicalKey::Character(logical.to_string()),
            text: text.map(|s| s.to_string()),
            location: bitty_platform::KeyLocation::Standard,
            state,
            repeat: false,
            is_synthetic: false,
        }
    }

    fn test_named_key(named: bitty_platform::NamedKey, state: PressState) -> KeyEvent {
        KeyEvent {
            logical_key: bitty_platform::LogicalKey::Named(named),
            text: None,
            location: bitty_platform::KeyLocation::Standard,
            state,
            repeat: false,
            is_synthetic: false,
        }
    }

    #[test]
    fn tracked_control_state_synthesizes_ctrl_bytes() {
        // CTX-0154: the legacy encoder must consult the tracked modifier
        // state, not winit text (None for Ctrl+letter on Wayland).
        let mut rt = make_runtime();
        // Pressing Control is modifier-only (no PTY bytes) but latches state.
        assert!(
            rt.handle_key_event(test_named_key(
                bitty_platform::NamedKey::Control,
                PressState::Pressed
            ))
            .is_none()
        );
        assert_eq!(rt.pending_input_len(), 0);
        // Wayland-style Ctrl+F (text=None) synthesizes 0x06.
        assert_eq!(
            rt.handle_key_event(test_char_key("f", None, PressState::Pressed)),
            Some(vec![0x06])
        );
        assert_eq!(rt.pending_input(), b"\x06");
        assert_eq!(rt.drain_pending_input(), b"\x06");
        // Bare-letter text under held Control synthesizes identically.
        assert_eq!(
            rt.handle_key_event(test_char_key("c", Some("c"), PressState::Pressed)),
            Some(vec![0x03])
        );
        assert_eq!(rt.drain_pending_input(), b"\x03");
        // Releasing Control unlatches: plain letters pass through again.
        assert!(
            rt.handle_key_event(test_named_key(
                bitty_platform::NamedKey::Control,
                PressState::Released
            ))
            .is_none()
        );
        assert_eq!(
            rt.handle_key_event(test_char_key("f", None, PressState::Pressed)),
            Some(b"f".to_vec())
        );
        assert_eq!(rt.drain_pending_input(), b"f");
        // Alt tracked via ModifiersChanged prefixes ESC (metaSendsEscape).
        let alt_on = PlatformEvent::Window {
            window_id: bitty_platform::WindowId::from_raw_public(7),
            kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
                shift: false,
                control: false,
                alt: true,
                super_pressed: false,
            }),
        };
        assert!(!rt.handle_platform_event(alt_on));
        assert_eq!(
            rt.handle_key_event(test_char_key("x", Some("x"), PressState::Pressed)),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(rt.drain_pending_input(), b"\x1bx");
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
    fn focus_gain_and_resumed_force_full_redraw() {
        let mut rt = make_runtime();
        assert!(rt.tick().is_some(), "first tick presents");
        assert!(rt.tick().is_none(), "idle when no damage");
        rt.set_focused(false);
        assert!(rt.tick().is_none(), "focus loss alone stays idle");
        rt.set_focused(true);
        assert!(rt.tick().is_some(), "focus gain repaints");
        assert!(rt.tick().is_none(), "idle again after focus repaint");
        assert!(!rt.handle_platform_event(PlatformEvent::Resumed));
        assert!(rt.tick().is_some(), "resume repaints");
    }

    // ------------------------------------------------------------------
    // CTX-0142: DPI adoption (scale-change -> recompute -> grid derivation)
    // ------------------------------------------------------------------

    #[test]
    fn dpi_adoption_derives_grid_from_physical_over_scaled_cells() {
        let mut rt = make_runtime();
        assert_eq!(rt.dpi_scale(), 1.0);
        // Hyprland scale 1.6, tiled physical extent 2506x1496: scaled cells
        // are 14x30 (CTX-0157 readable 9x19 base), so the grid must be
        // 179x49 (not 278x78 unscaled).
        rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2506, 1496)));
        assert_eq!(rt.dpi_scale(), 1.6);
        let snap = rt.snapshot();
        assert_eq!((snap.width, snap.height), (179, 49));
        assert_eq!(
            rt.surface_extent(),
            Some(PhysicalSize::new(2506, 1496)),
            "surface must be reconfigured to the physical extent"
        );
        assert!(rt.tick().is_some(), "adoption forces full redraw");
    }

    #[test]
    fn dpi_rescale_without_extent_keeps_grid_for_following_resized() {
        let mut rt = make_runtime();
        let before = rt.snapshot();
        rt.apply_dpi_scale(1.6, None);
        assert_eq!(rt.dpi_scale(), 1.6, "renderer rescales even without extent");
        let kept = rt.snapshot();
        assert_eq!(
            (kept.width, kept.height),
            (before.width, before.height),
            "grid waits for the physical extent"
        );
        // The following Resized takes precedence and derives from the same
        // scaled cells (proves Resized-after-scale consistency).
        rt.handle_resize(PhysicalSize::new(2506, 1496))
            .expect("valid resize");
        let snap = rt.snapshot();
        assert_eq!((snap.width, snap.height), (179, 49));
    }

    #[test]
    fn invalid_dpi_scales_are_sanitized_fail_safe() {
        for invalid in [0.0, -1.6, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut rt = make_runtime();
            // Must never panic or strand the window.
            rt.apply_dpi_scale(invalid, Some(PhysicalSize::new(800, 600)));
            assert_eq!(
                rt.dpi_scale(),
                1.0,
                "invalid scale {invalid:?} must sanitize to 1.0"
            );
            let snap = rt.snapshot();
            assert_eq!(
                (snap.width, snap.height),
                (88, 31),
                "unscaled 9x19 cells over 800x600"
            );
            assert_eq!(rt.surface_extent(), Some(PhysicalSize::new(800, 600)));
            assert!(rt.tick().is_some(), "window stays drawable");
        }
        // Hostile magnitudes clamp instead of exploding geometry.
        let mut rt = make_runtime();
        rt.apply_dpi_scale(100.0, Some(PhysicalSize::new(800, 600)));
        assert_eq!(rt.dpi_scale(), 4.0);
        let snap = rt.snapshot();
        assert!(snap.width >= 1 && snap.height >= 1);
        let mut rt = make_runtime();
        rt.apply_dpi_scale(0.01, Some(PhysicalSize::new(800, 600)));
        assert_eq!(rt.dpi_scale(), 0.25);
    }

    #[test]
    fn zero_extent_dpi_adoption_rescales_but_skips_reflow() {
        let mut rt = make_runtime();
        let surface_before = rt.surface_extent();
        let grid_before = {
            let snap = rt.snapshot();
            (snap.width, snap.height)
        };
        rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(0, 0)));
        assert_eq!(rt.dpi_scale(), 1.6, "renderer still rescales");
        let snap = rt.snapshot();
        assert_eq!(
            (snap.width, snap.height),
            grid_before,
            "zero extent (minimized) must not reflow the grid"
        );
        assert_eq!(rt.surface_extent(), surface_before);
    }

    #[test]
    fn scale_factor_changed_event_adopts_without_exit() {
        let mut rt = make_runtime();
        let exit = rt.handle_platform_event(PlatformEvent::Window {
            window_id: bitty_platform::WindowId::from_raw_public(1),
            kind: WindowEventKind::ScaleFactorChanged(ScaleFactor::new(1.6).expect("valid")),
        });
        assert!(!exit, "scale change must not request exit");
        assert_eq!(rt.dpi_scale(), 1.6);
        assert!(rt.tick().is_some(), "scale change forces full redraw");
        // Invalid factors through the event path sanitize, never panic.
        let exit = rt.handle_platform_event(PlatformEvent::Window {
            window_id: bitty_platform::WindowId::from_raw_public(1),
            kind: WindowEventKind::ScaleFactorChanged(ScaleFactor::new_sanitized(f64::NAN)),
        });
        assert!(!exit);
        assert_eq!(rt.dpi_scale(), 1.0);
    }

    #[test]
    fn logical_recompute_matches_physical_resize() {
        // Embedders holding only cached logical geometry convert via
        // surface_extent_from_logical first: 1566x935 logical at 1.6x must
        // reach the identical grid as the physical Resized path (2506x1496).
        let logical = bitty_platform::LogicalSize::new(1566.0, 935.0).expect("valid");
        let scale = ScaleFactor::new(1.6).expect("valid");
        let physical =
            bitty_platform::surface_extent_from_logical(logical, scale).expect("non-zero extent");
        assert_eq!(physical, PhysicalSize::new(2506, 1496));
        let mut via_logical = make_runtime();
        via_logical.apply_dpi_scale(1.6, Some(physical));
        let mut via_physical = make_runtime();
        via_physical.apply_dpi_scale(1.6, None);
        via_physical.handle_resize(physical).expect("valid resize");
        let a = via_logical.snapshot();
        let b = via_physical.snapshot();
        assert_eq!((a.width, a.height), (179, 49));
        assert_eq!((b.width, b.height), (179, 49));
    }

    #[test]
    fn repeated_rescales_start_from_design_base_without_drift() {
        let mut rt = make_runtime();
        rt.apply_dpi_scale(2.0, Some(PhysicalSize::new(1600, 1200)));
        let scaled = rt.snapshot();
        // 9x19 base at 2x -> 18x38 cells: 1600/18=88, 1200/38=31.
        assert_eq!((scaled.width, scaled.height), (88, 31));
        // Back to 1.0 must restore the exact base grid, not a rounded echo.
        rt.apply_dpi_scale(1.0, Some(PhysicalSize::new(1600, 1200)));
        let restored = rt.snapshot();
        assert_eq!((restored.width, restored.height), (177, 63));
        assert_eq!(rt.dpi_scale(), 1.0);
    }

    #[test]
    fn resized_after_scale_uses_scaled_cells() {
        let mut rt = make_runtime();
        rt.apply_dpi_scale(2.0, None);
        rt.handle_resize(PhysicalSize::new(800, 600))
            .expect("valid resize");
        let snap = rt.snapshot();
        // 18x38 scaled cells: 800/18=44, 600/38=15 (unscaled would be 88x31).
        assert_eq!((snap.width, snap.height), (44, 15));
    }

    #[test]
    fn present_plan_extent_matches_draw_list_pixels_so_gpu_scale_stays_near_one() {
        use bitty_render::batch::derive_scale;

        let mut rt = make_runtime();
        rt.apply_dpi_scale(1.6, Some(PhysicalSize::new(2506, 1496)));
        let surface = rt.surface_extent().expect("adopted surface");
        let plan = rt.present_plan_extent();
        // 179x49 grid at 14x30 scaled cells = 2506x1470 draw-list pixels.
        assert_eq!((plan.width, plan.height), (179 * 14, 49 * 30));
        let scale = derive_scale(surface.width(), surface.height(), plan);
        assert!(
            (scale - 1.0).abs() < 0.05,
            "adopted frame must present near 1.0, got {scale}"
        );
        // The stale 1-cell probe extent this replaced clamped to 4x
        // magnification (the dominant blur behind #232).
        let stale = bitty_render::geometry::ExtentPx::new(14, 30);
        assert_eq!(
            derive_scale(surface.width(), surface.height(), stale),
            4.0,
            "1-cell plan extent magnifies 4x"
        );
    }

    #[test]
    fn present_plan_extent_tracks_scale_one_frames() {
        use bitty_render::batch::derive_scale;

        let rt = make_runtime();
        let surface = rt.surface_extent().expect("default surface");
        let plan = rt.present_plan_extent();
        // Default 80x24 grid at 9x19 readable cells = 720x456.
        assert_eq!((plan.width, plan.height), (720, 456));
        assert_eq!(derive_scale(surface.width(), surface.height(), plan), 1.0);
    }

    #[test]
    fn spawn_shell_blank_program_rejected_without_touching_pty() {
        let mut rt = make_runtime();
        assert!(rt.spawn_shell("").is_err());
        assert!(rt.spawn_shell("   ").is_err());
    }

    #[test]
    fn pane_sessions_default_to_empty() {
        // CTX-0176: single-pane runtimes own no pane session; the primary
        // PTY/state path is untouched.
        let mut rt = make_runtime();
        assert_eq!(rt.pane_count(), 0);
        assert!(rt.pane_session_ids().is_empty());
        assert!(!rt.has_pane_session(&ViewId::new(2)));
        assert_eq!(rt.pane_pid(&ViewId::new(2)), None);
        assert!(rt.pane_snapshot(&ViewId::new(2)).is_none());
        assert!(!rt.close_pane_session(&ViewId::new(2)));
    }

    #[test]
    fn pane_spawn_rejects_blank_program_and_unknown_view() {
        // CTX-0176: validation precedes any spawn; no PTY is touched.
        let mut rt = make_runtime();
        assert!(
            rt.spawn_shell_for_view(ViewId::new(1), "", &[], 80, 24)
                .is_err()
        );
        assert!(
            rt.spawn_shell_for_view(ViewId::new(1), "   ", &[], 80, 24)
                .is_err()
        );
        assert!(!rt.has_pty());
        assert_eq!(rt.pane_count(), 0);
        // Leaf 9 is not in the single-leaf default layout.
        assert!(
            rt.spawn_shell_for_view(ViewId::new(9), "/bin/sh", &[], 80, 24)
                .is_err()
        );
        assert_eq!(rt.pane_count(), 0);
    }

    #[test]
    fn pane_pump_and_poll_are_noops_without_sessions() {
        // CTX-0176: unknown ids never touch the primary pipeline, polling an
        // unspawned runtime still drains nothing, and the single-pane input
        // path still lands in the headless buffer.
        let mut rt = make_runtime();
        rt.handle_pane_bytes(ViewId::new(2), b"hello");
        rt.handle_pane_bytes(ViewId::new(2), b"");
        assert_eq!(rt.poll_pty(), 0);
        rt.push_input_bytes(b"abc");
        assert_eq!(rt.pending_input(), b"abc");
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
        // Resize to 800x600 pixels with readable cell 9x19 => 88x31 cells
        rt.handle_resize(PhysicalSize::new(800, 600))
            .expect("resize");
        assert_eq!(rt.container(), UiRect::new(0, 0, 88, 31));
        let allocs = rt.layout_allocations();
        // Horizontal split of 88 -> 44 each
        assert_eq!(allocs[0].1.width, 44);
        assert_eq!(allocs[1].1.width, 44);
        assert_eq!(allocs[0].1.height, 31);
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
        // Tick must still present via headless seam (surface is still 720x456,
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
        let cap = CapabilityId::parse("terminal.semantic-read").unwrap();
        let mut m = host_manifest("xuepoo.test", vec!["xuepoo.test:cmd"], vec![]);
        m.capabilities.ids.insert(cap.clone());
        rt.register_plugin(m).unwrap();
        let pid = HostPid::new("xuepoo.test").unwrap();
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
        let cap = CapabilityId::parse("ui.rich").unwrap();
        let mut m = host_manifest("xuepoo.test", vec!["xuepoo.test:run"], vec![]);
        m.capabilities.ids.insert(cap.clone());
        rt.register_plugin(m).unwrap();
        let pid = HostPid::new("xuepoo.test").unwrap();
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
        assert!(!Runtime::intercept_open_url(&[HostDecision::Approve], true));
        assert!(Runtime::intercept_terminal_spawn(&[], false));
    }

    #[test]
    fn opening_url_requires_gesture_and_interception_approval() {
        let mut rt = make_runtime();
        let denied = rt.authorize_url_activation(
            "https://example.test",
            ActivationGesture(0),
            &[HostDecision::Approve],
            false,
        );
        assert_eq!(
            denied,
            Err(bitty_platform::PlatformError::UrlActivationDenied)
        );
    }

    #[test]
    fn platform_hyperlink_activation_mints_single_use_gesture() {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
        let window_id = bitty_platform::WindowId::from_raw_public(1);
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
        });
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        let gesture = rt
            .take_activation_gesture()
            .expect("runtime must mint gesture");
        let activation = rt
            .authorize_url_activation("https://example.test", gesture, &[], false)
            .expect("platform gesture must authorize safe hyperlink");
        assert_eq!(activation.uri, "https://example.test");
        assert!(
            rt.take_activation_gesture().is_none(),
            "gesture is single-use"
        );
    }

    #[test]
    fn opening_url_validates_after_activation_gate() {
        let mut rt = make_runtime();
        let result =
            rt.authorize_url_activation("javascript:alert(1)", ActivationGesture(0), &[], false);
        assert_eq!(
            result,
            Err(bitty_platform::PlatformError::UrlActivationDenied)
        );
    }

    #[test]
    fn file_urls_require_distinct_approval() {
        let mut rt = make_runtime();
        assert!(
            rt.authorize_url_activation("file:///tmp/report", ActivationGesture(0), &[], false)
                .is_err()
        );
        assert!(
            rt.authorize_file_url_activation(
                "file:///tmp/report",
                ActivationGesture(0),
                &[],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_output_alone_cannot_activate() {
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"file:///tmp/report");
        assert!(
            rt.authorize_file_url_activation(
                "file:///tmp/report",
                ActivationGesture(0),
                &[],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn file_authority_is_rejected_before_authorization_token_is_issued() {
        let mut rt = make_runtime();
        assert_eq!(
            rt.authorize_file_url_activation(
                "file://server/share",
                ActivationGesture(0),
                &[],
                false
            ),
            Err(bitty_platform::PlatformError::UrlActivationDenied)
        );
    }

    #[test]
    fn hostile_hyperlink_does_not_consume_gesture_slot() {
        let mut rt = make_runtime();
        // Hostile URI should not mint a gesture.
        rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07link\x1b]8;;\x07");
        let window_id = bitty_platform::WindowId::from_raw_public(1);
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
        });
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        assert!(
            rt.take_activation_gesture().is_none(),
            "hostile hyperlink must not mint gesture"
        );
        // Safe hyperlink after hostile must still mint.
        let mut rt2 = make_runtime();
        rt2.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
        rt2.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
        });
        rt2.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        assert!(
            rt2.take_activation_gesture().is_some(),
            "safe hyperlink must mint gesture even after hostile attempt in separate runtime"
        );
    }

    #[test]
    fn hostile_then_safe_in_same_runtime_preserves_gesture_for_safe() {
        let mut rt = make_runtime();
        let window_id = bitty_platform::WindowId::from_raw_public(2);
        // First, hostile.
        rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
        });
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        assert!(rt.take_activation_gesture().is_none());
        // Then safe link overwriting same cell (carriage return to col 0).
        rt.handle_pty_bytes(b"\r\x1b]8;;https://example.test\x07y\x1b]8;;\x07");
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition { x: 1.0, y: 1.0 }),
        });
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        let gesture = rt
            .take_activation_gesture()
            .expect("safe must mint after hostile");
        let ok = rt.authorize_url_activation("https://example.test", gesture, &[], false);
        assert!(ok.is_ok());
    }

    #[test]
    fn hyperlink_activation_overflow_is_handled_without_panic() {
        let mut rt = make_runtime();
        // Force a large snapshot width via state resize to max config-allowed, then
        // verify checked arithmetic does not panic on extreme cursor.
        // The runtime clamps cursor_to_cell, so overflow is defensive.
        let window_id = bitty_platform::WindowId::from_raw_public(3);
        rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
        // Cursor far outside window should clamp, not overflow.
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::CursorMoved(CursorPosition {
                x: f64::MAX,
                y: f64::MAX,
            }),
        });
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(bitty_platform::MouseEvent {
                button: MouseButton::Left,
                state: PressState::Released,
            }),
        });
        // Should not panic; may or may not mint depending on clamped cell.
        let _ = rt.take_activation_gesture();
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

    // CTX-0158 mouse selection/clipboard: ghostty semantics, headless only.
    //
    // Every test forces the headless clipboard seam first, so no test touches
    // the real display server clipboard or primary. Pure/total, no fs/net.

    fn mouse_headless_runtime(text: &str) -> Runtime {
        let mut rt = make_runtime();
        rt.force_headless_clipboard();
        rt.handle_pty_bytes(text.as_bytes());
        rt
    }

    fn mouse_headless_runtime_no_auto_copy(text: &str) -> Runtime {
        // CTX-0191: runtime with the opt-out toggle off.
        let mut rt = Runtime::new(RuntimeConfig {
            selection_auto_copy: false,
            ..RuntimeConfig::default()
        })
        .expect("opt-out runtime must build");
        rt.force_headless_clipboard();
        rt.handle_pty_bytes(text.as_bytes());
        rt
    }

    fn mouse_press(button: bitty_platform::MouseButton) -> bitty_platform::MouseEvent {
        bitty_platform::MouseEvent {
            button,
            state: PressState::Pressed,
        }
    }

    fn mouse_release(button: bitty_platform::MouseButton) -> bitty_platform::MouseEvent {
        bitty_platform::MouseEvent {
            button,
            state: PressState::Released,
        }
    }

    #[test]
    fn left_release_auto_copies_to_clipboard_and_primary() {
        let mut rt = mouse_headless_runtime("hello world");
        assert_eq!(rt.clipboard().headless_contents(), "");
        assert_eq!(rt.primary_contents(), "");
        // Drag cells (0,0)..(0,4) = "hello" via the mouse path.
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Left));
        rt.handle_cursor_moved(CursorPosition {
            x: 9.0 * 4.0,
            y: 0.0,
        });
        rt.handle_mouse_input(mouse_release(MouseButton::Left));
        assert!(!rt.is_selection_dragging());
        assert_eq!(rt.selection_text().as_deref(), Some("hello"));
        // Ghostty copy-on-select: both clipboards hold the selection.
        assert_eq!(rt.clipboard().headless_contents(), "hello");
        assert_eq!(rt.primary_contents(), "hello");
        // A successful copy clears any recorded clipboard failure.
        assert!(rt.last_clipboard_error().is_none());
    }

    #[test]
    fn left_release_with_auto_copy_off_highlights_without_copying() {
        // CTX-0191: opt-out leaves the highlight in place but touches
        // neither clipboard; the explicit chord path still copies.
        assert!(
            !RuntimeConfig {
                selection_auto_copy: false,
                ..RuntimeConfig::default()
            }
            .validate()
            .is_err()
        );
        let mut rt = mouse_headless_runtime_no_auto_copy("hello world");
        assert!(!rt.config().selection_auto_copy);
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Left));
        rt.handle_cursor_moved(CursorPosition {
            x: 9.0 * 4.0,
            y: 0.0,
        });
        rt.handle_mouse_input(mouse_release(MouseButton::Left));
        // Highlight present, drag finished, clipboards untouched.
        assert!(!rt.is_selection_dragging());
        assert!(rt.has_selection());
        assert_eq!(rt.selection_text().as_deref(), Some("hello"));
        assert_eq!(rt.clipboard().headless_contents(), "");
        assert_eq!(rt.primary_contents(), "");
        // Explicit chord path (Ctrl+Shift+C arm) still copies on demand.
        let copied = rt
            .copy_selection_to_clipboard()
            .expect("explicit copy must not error");
        assert_eq!(copied.as_deref(), Some("hello"));
        assert_eq!(rt.clipboard().headless_contents(), "hello");
    }

    #[test]
    fn left_release_auto_copy_overwrites_divergent_primary() {
        // Regression pin for the live Wayland gap (select-in-bitty never
        // reached `wl-paste --primary`): `auto_copy_selection` must replace
        // a stale/divergent primary with the new selection, not just write
        // the regular clipboard. The write itself is delivered by the
        // platform layer's wl-copy-first primary sync (CTX-0160 as fixed
        // here); the headless seam proves the contract deterministically.
        let mut rt = mouse_headless_runtime("hello world");
        rt.clipboard_mut()
            .set_text("zz".to_string())
            .expect("headless set");
        rt.set_primary_text("pq".to_string());
        assert_eq!(rt.clipboard().headless_contents(), "zz");
        assert_eq!(rt.primary_contents(), "pq");
        // Drag cells (0,0)..(0,4) = "hello" via the mouse path.
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Left));
        rt.handle_cursor_moved(CursorPosition {
            x: 9.0 * 4.0,
            y: 0.0,
        });
        rt.handle_mouse_input(mouse_release(MouseButton::Left));
        assert_eq!(rt.selection_text().as_deref(), Some("hello"));
        assert_eq!(rt.clipboard().headless_contents(), "hello");
        assert_eq!(rt.primary_contents(), "hello");
        assert!(rt.last_clipboard_error().is_none());
    }

    #[test]
    fn left_release_without_drag_copies_nothing() {
        let mut rt = mouse_headless_runtime("hello world");
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Left));
        rt.handle_mouse_input(mouse_release(MouseButton::Left));
        assert!(!rt.has_selection());
        assert_eq!(rt.clipboard().headless_contents(), "");
        assert_eq!(rt.primary_contents(), "");
    }

    #[test]
    fn right_click_pastes_clipboard_bytes() {
        let mut rt = mouse_headless_runtime("hello world");
        rt.clipboard_mut()
            .set_text("hi".to_string())
            .expect("headless set");
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Right));
        assert!(!rt.has_pending_paste(), "clean paste needs no confirm");
        assert_eq!(rt.pending_input(), b"hi");
        // A successful read leaves no recorded clipboard failure.
        assert!(rt.last_clipboard_error().is_none());
        // Release is a no-op: exactly one paste per click.
        rt.handle_mouse_input(mouse_release(MouseButton::Right));
        assert_eq!(rt.pending_input(), b"hi");
    }

    #[test]
    fn right_click_with_empty_clipboard_pastes_nothing_without_error() {
        let mut rt = mouse_headless_runtime("hello world");
        assert!(rt.last_clipboard_error().is_none());
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Right));
        assert!(rt.pending_input().is_empty());
        assert!(!rt.has_pending_paste());
        assert!(rt.last_clipboard_error().is_none());
    }

    #[test]
    fn middle_click_pastes_primary_bytes() {
        let mut rt = mouse_headless_runtime("hello world");
        // Standard clipboard holds something else: middle must read primary.
        // Order matters: the platform `set_text` best-effort syncs the
        // primary selection (CTX-0160 ghostty copy-on-select), so stage the
        // standard clipboard first and the primary second.
        rt.clipboard_mut()
            .set_text("zz".to_string())
            .expect("headless set");
        rt.set_primary_text("pq".to_string());
        assert_eq!(rt.clipboard().headless_contents(), "zz");
        assert_eq!(rt.primary_contents(), "pq");
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Middle));
        assert!(!rt.has_pending_paste());
        assert_eq!(rt.pending_input(), b"pq");
        assert!(rt.last_clipboard_error().is_none());
        rt.handle_mouse_input(mouse_release(MouseButton::Middle));
        assert_eq!(rt.pending_input(), b"pq");
        // Right-click still reads the standard clipboard, not primary.
        rt.drain_pending_input();
        rt.handle_mouse_input(mouse_press(MouseButton::Right));
        assert!(!rt.has_pending_paste());
        assert_eq!(rt.pending_input(), b"zz");
    }

    #[test]
    fn clipboard_copy_syncs_primary_like_ghostty() {
        // Platform CTX-0160 contract through the runtime seam: a standard
        // clipboard write also lands in the primary selection, so a
        // left-drag auto-copy is middle-pasteable without a second write.
        let mut rt = mouse_headless_runtime("hello world");
        rt.clipboard_mut()
            .set_text("synced".to_string())
            .expect("headless set");
        assert_eq!(rt.primary_contents(), "synced");
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Middle));
        assert_eq!(rt.pending_input(), b"synced");
    }

    #[test]
    fn middle_click_with_empty_primary_pastes_nothing() {
        // Fresh headless seam: both buffers start empty, so the primary read
        // succeeds empty (no error recorded) and pastes nothing.
        let mut rt = mouse_headless_runtime("hello world");
        assert_eq!(rt.primary_contents(), "");
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Middle));
        assert!(rt.pending_input().is_empty());
        assert!(!rt.has_pending_paste());
        assert!(rt.last_clipboard_error().is_none());
    }

    #[test]
    fn suspicious_right_paste_waits_for_confirmation() {
        let mut rt = mouse_headless_runtime("hello world");
        rt.clipboard_mut()
            .set_text("a\nb".to_string())
            .expect("headless set");
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        rt.handle_mouse_input(mouse_press(MouseButton::Right));
        assert!(rt.has_pending_paste(), "newline paste needs confirm");
        assert!(rt.pending_input().is_empty(), "no silent delivery");
        assert!(rt.confirm_pending_paste(true));
        assert_eq!(rt.pending_input(), b"a\nb");
    }

    #[test]
    fn capture_mode_reports_sgr_and_never_pastes() {
        let mut rt = mouse_headless_runtime("hello world");
        rt.handle_pty_bytes(b"\x1b[?1000h");
        rt.handle_pty_bytes(b"\x1b[?1006h");
        rt.clipboard_mut()
            .set_text("clip".to_string())
            .expect("headless set");
        rt.set_primary_text("prim".to_string());
        rt.drain_pending_input();
        rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
        // Right press in capture must report SGR, not paste.
        rt.handle_mouse_input(mouse_press(MouseButton::Right));
        assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M");
        assert!(!rt.has_pending_paste());
        assert!(!rt.has_selection(), "capture must not select");
        // Middle press likewise reports its own button code.
        rt.handle_mouse_input(mouse_press(MouseButton::Middle));
        assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M\x1b[<1;1;1M");
        assert!(!rt.has_pending_paste());
        // Left release in capture reports SGR and never auto-copies.
        rt.handle_mouse_input(mouse_release(MouseButton::Left));
        assert_eq!(rt.pending_input(), b"\x1b[<2;1;1M\x1b[<1;1;1M\x1b[<0;1;1m");
        assert_eq!(rt.clipboard().headless_contents(), "clip");
        assert_eq!(rt.primary_contents(), "prim");
        assert!(rt.last_clipboard_error().is_none());
    }

    #[test]
    fn wheel_up_scrolls_into_history_and_down_returns_to_live() {
        // CTX-0155 (#251): winit LineDelta/PixelDelta y>0 = wheel up;
        // View::scroll_by positive = up into history. Wheel-up from live
        // must increase offset; wheel-down must decrease it.
        // CTX-0185: one notch now moves `scroll_lines_per_notch` (default 3),
        // not 1 — direction semantics unchanged, throughput fixed.
        let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;
        let pixels_per_notch = RuntimeConfig::default().scroll_pixels_per_notch as f64;
        let mut rt = make_runtime();
        for i in 0..60 {
            let line = format!("line {i:02}\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        assert!(
            rt.state.scrollback_len() > 5,
            "need scrollback for wheel test, got {}",
            rt.state.scrollback_len()
        );
        let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
        let offset = |rt: &Runtime| {
            rt.layout
                .find_leaf(view_id)
                .map(|v| v.scroll_offset())
                .unwrap_or(usize::MAX)
        };
        assert_eq!(offset(&rt), 0, "must start at live");
        // Lines path.
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
        assert_eq!(
            offset(&rt),
            lines_per_notch,
            "wheel-up (Lines y>0) must go into history by one notch"
        );
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -1.0));
        assert_eq!(
            offset(&rt),
            0,
            "wheel-down (Lines y<0) must return toward live"
        );
        // Pixels path (accumulator threshold = configured pixels per notch).
        rt.handle_wheel(bitty_platform::ScrollDelta::Pixels(
            0.0,
            pixels_per_notch * 2.0,
        ));
        assert_eq!(
            offset(&rt),
            lines_per_notch * 2,
            "wheel-up (Pixels py>0) must go into history by two notches"
        );
        rt.handle_wheel(bitty_platform::ScrollDelta::Pixels(
            0.0,
            -(pixels_per_notch * 2.0),
        ));
        assert_eq!(
            offset(&rt),
            0,
            "wheel-down (Pixels py<0) must return toward live"
        );
    }

    #[test]
    fn wheel_fractional_line_deltas_accumulate_instead_of_dropping() {
        // CTX-0185: high-resolution wheels emit fractional LineDelta notches
        // (|y| < 1.0). Truncating each event to `isize` dropped them outright
        // (read as lag); they must bank across events. Default 3 lines/notch:
        // 0.25 notch = 0.75 lines banked, second 0.25 completes 1.5 -> 1 line.
        let mut rt = make_runtime();
        for i in 0..60 {
            let line = format!("line {i:02}\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
        let offset = |rt: &Runtime| {
            rt.layout
                .find_leaf(view_id)
                .map(|v| v.scroll_offset())
                .unwrap_or(usize::MAX)
        };
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 0.25));
        assert_eq!(offset(&rt), 0, "sub-line fraction must not scroll yet");
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 0.25));
        assert_eq!(offset(&rt), 1, "banked fractions must complete a line");
        // Opposite fractions walk back down (direction preserved).
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -0.25));
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, -0.25));
        assert_eq!(offset(&rt), 0, "fractions must unwind toward live");
    }

    #[test]
    fn wheel_scroll_speed_config_scales_per_notch_distance() {
        // CTX-0185: per-notch distance is configurable and validated.
        fn runtime_with_scroll(lines: u32, pixels: u32) -> Runtime {
            Runtime::new(RuntimeConfig {
                scroll_lines_per_notch: lines,
                scroll_pixels_per_notch: pixels,
                ..RuntimeConfig::default()
            })
            .expect("custom scroll speed must build")
        }
        fn fill(rt: &mut Runtime) {
            for i in 0..60 {
                let line = format!("line {i:02}\n");
                rt.handle_pty_bytes(line.as_bytes());
            }
        }
        fn offset_of(rt: &Runtime) -> usize {
            let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
            rt.layout
                .find_leaf(view_id)
                .map(|v| v.scroll_offset())
                .unwrap_or(usize::MAX)
        }
        // 1 line/notch restores the pre-CTX-0185 feel exactly.
        let mut slow = runtime_with_scroll(1, 16);
        fill(&mut slow);
        slow.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
        assert_eq!(offset_of(&slow), 1);
        // 5 lines/notch moves five times further per event.
        let mut fast = runtime_with_scroll(5, 16);
        fill(&mut fast);
        fast.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
        assert_eq!(offset_of(&fast), 5);
        // Pixels threshold is configurable: 8px/notch means 16px = 2 notches.
        let mut touchy = runtime_with_scroll(2, 8);
        fill(&mut touchy);
        touchy.handle_wheel(bitty_platform::ScrollDelta::Pixels(0.0, 16.0));
        assert_eq!(offset_of(&touchy), 4);
    }

    #[test]
    fn wheel_fling_coalesces_to_one_present_then_idles() {
        // CTX-0185 profile evidence: wheel events only set
        // `pending_full_redraw`; an N-event fling without intermediate ticks
        // costs exactly one present, and the next tick idles (frame-on-demand
        // preserved — scroll adds no wakeups).
        let mut rt = make_runtime();
        for i in 0..60 {
            let line = format!("line {i:02}\n");
            rt.handle_pty_bytes(line.as_bytes());
        }
        for _ in 0..5 {
            rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
        }
        let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
        let offset = rt
            .layout
            .find_leaf(view_id)
            .map(|v| v.scroll_offset())
            .unwrap_or(usize::MAX);
        assert_eq!(
            offset,
            5 * RuntimeConfig::default().scroll_lines_per_notch as usize
        );
        assert!(
            rt.tick().is_some(),
            "fling must present exactly once on the next tick"
        );
        assert!(
            rt.tick().is_none(),
            "must idle after presenting (no scroll wakeups)"
        );
    }

    #[test]
    fn wheel_sgr_capture_scales_with_scroll_speed() {
        // CTX-0185: mouse-mode SGR wheel emission scales with the configured
        // lines/notch (one SGR event per line, still capped at 32/frame) and
        // never scrolls the viewport.
        let mut rt = make_runtime();
        rt.handle_pty_bytes(b"\x1b[?1000h");
        rt.handle_pty_bytes(b"\x1b[?1006h");
        rt.drain_pending_input();
        rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 1.0));
        let pending = String::from_utf8_lossy(rt.pending_input()).into_owned();
        let ups = pending.matches("\x1b[<64;").count();
        assert_eq!(
            ups,
            RuntimeConfig::default().scroll_lines_per_notch as usize,
            "one SGR 64 per line in the notch, got {pending:?}"
        );
        let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
        let offset = rt
            .layout
            .find_leaf(view_id)
            .map(|v| v.scroll_offset())
            .unwrap_or(usize::MAX);
        assert_eq!(offset, 0, "capture scroll must not move the viewport");
    }

    #[test]
    fn selection_highlight_renders_end_to_end() {
        // Render-side primitive: one opaque row rect in the theme color.
        let cell = bitty_render::grid::CellMetrics::new(8, 16).expect("non-zero cell");
        let single = bitty_render::grid::selection_fill_rects((0, 0), (0, 4), 80, 24, cell);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].color, bitty_render::grid::selection_fill());
        assert_eq!(single[0].rect.x, 0);
        assert_eq!(single[0].rect.y, 0);
        assert_eq!(single[0].rect.width, 5 * 8);
        assert_eq!(single[0].rect.height, 16);
        // Multi-row spans one rect per row, row-major.
        let multi = bitty_render::grid::selection_fill_rects((0, 1), (1, 1), 80, 24, cell);
        assert_eq!(multi.len(), 2);
        assert!(
            multi
                .iter()
                .all(|f| f.color == bitty_render::grid::selection_fill())
        );
        // Collapsed and empty grids paint nothing.
        assert!(bitty_render::grid::selection_fill_rects((2, 2), (2, 2), 80, 24, cell).is_empty());
        assert!(bitty_render::grid::selection_fill_rects((0, 0), (0, 4), 0, 24, cell).is_empty());

        // Runtime end-to-end: a committed selection forces the next tick to
        // present (selection-only changes bump the generation gate via
        // pending_full_redraw, otherwise the highlight would never paint).
        let mut rt = mouse_headless_runtime("hello world");
        assert!(rt.tick().is_some(), "first tick presents");
        assert_eq!(rt.tick(), None, "idle with no changes");
        rt.start_selection(bitty_ui::CellPos::new(0, 0));
        rt.end_selection(bitty_ui::CellPos::new(0, 4));
        let stats = rt.tick().expect("selection must force a present");
        assert!(stats.fills > 0);
        assert!(rt.headless_rgba().is_some());
    }
}
