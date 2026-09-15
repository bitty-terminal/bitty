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
//!                                  +--> bounded side queue --> PluginHost (accepted contracts)
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
//! session. The runtime-global primary grid belongs to exactly one leaf, the
//! `primary_view` (CTX-0359: the leaf focused when the primary shell
//! attached); every other session-less leaf renders erased. Input routes to
//! the focused leaf's own writer only, falling back to the primary writer
//! exclusively on the primary owner (see [`Runtime::push_input_bytes`]);
//! [`Runtime::tick`] renders each leaf from its own grid; [`Runtime::poll_pty`]
//! drains every session. [`Runtime::close_pane_session`] tears a leaf's child
//! down.
//!
//! # Plugin-host wiring (CTX-0027) — accepted contracts, implementation not yet verified
//!
//! This module owns a [`bitty_plugin_host::PluginHost`] behind the cold path.
//! The host tracks two accepted contracts: `plugin-platform-rfc.md`
//! (`accepted` 2026-08-27, closed `OQ-011`/`OQ-012`/`OQ-013`) and
//! `isolation-resource-rfc.md` (`accepted` 2026-08-28, closed `OQ-014`)
//! (bitty-docs open-questions register). The wiring is headless-testable
//! and introduces no window, GPU, or Lua VM coupling:
//!
//! - **Owned host:** `Runtime` owns one `PluginHost` (always present, not feature-gated
//!   for this slice). Construction uses the **accepted v1 default**
//!   [`bitty_plugin_host::DropPolicy::DropOldest`] with per-queue `64` and side queue `128`
//!   per the accepted Plugin Platform RFC (2026-08-27, frontmatter `status: accepted`;
//!   bitty-docs open-questions register).
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
    CrossFontRasterizer, FallbackRasterizer, RenderError,
    frame::{FrameMode, FramePlan},
    glyph::{
        BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
        RasterKey,
    },
    gpu::{GpuContext, Surface},
    grid::{CellMetrics, DrawList, GridRenderer},
    grid_from_surface_extent, sanitize_dpi_scale,
};
use bitty_term_state::search::{SearchMatch, SearchOptions};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction};
use bitty_ui::{
    CellPos, Focus, FocusDirection, Gaps, LayoutNode, PersistentSelection, Rect as UiRect,
    SearchHighlight, Selection, SelectionKind, View, ViewId, search::SearchState,
};
use bitty_vt::{
    ClipboardOp, DynamicColorOp, DynamicColorTarget, PaletteColorOp, Parser, SequenceKind,
};

use bitty_plugin_host::{
    CapabilityId, DropPolicy, Event, EventKind, GrantRecord, HostObservation, InterceptionDecision,
    PluginHost, PluginId, PluginManifest, QualifiedName,
};

use crate::config::RuntimeConfig;
use crate::error::RuntimeError;
use crate::queue::{ColdEvent, ColdQueue};

pub mod animations;
pub mod background_images;
pub mod click;
pub mod close_confirm;
pub mod copy_mode;
pub mod help;
pub mod input;
pub mod kitty_images;
pub mod layout_focus;
pub mod mouse_chrome;
pub mod panes;
pub mod plugin;
pub mod present;
pub mod pty;
pub mod resize;
pub mod scrollbar;
pub mod search;
pub mod selection;
pub mod session;
pub mod workspaces;

pub use self::animations::{
    AnimationCurve, AnimationKind, AnimationPolicy, ClosingFrame, MAX_CONCURRENT_ANIMATIONS,
    PanelAnimator, ReducedMotionMode,
};
pub use self::kitty_images::{KittyDisplayOutcome, KittyImageError};
pub use self::present::{ImeCursorArea, PresentStats};

use self::click::ClickTracker;
use self::close_confirm::PendingCloseConfirm;
use self::layout_focus::{default_container, default_layout};
use self::mouse_chrome::{AltDragState, HoverPending};
use self::panes::PaneSession;
use self::present::{AnyRasterizer, HeadlessRasterizer, ImeCaret};
use self::scrollbar::ScrollbarDrag;
use self::workspaces::{PendingWsClose, WorkspaceSlot};

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
/// Accepted v1 defaults for the plugin-host wiring per the accepted contracts.
/// These satisfy bounded-queue invariants and are headless-testable; pipeline
/// `64` / side `128` and batch `32`/`8 KiB` are the accepted OQ-014 baseline
/// per the accepted Isolation Resource RFC (closed OQ-014 on 2026-08-28;
/// bitty-docs open-questions register), while the drop policy is OQ-013 closed
/// per the accepted Plugin Platform RFC.
pub const DEFAULT_PLUGIN_PIPELINE_CAPACITY: usize = bitty_plugin_host::DEFAULT_QUEUE_CAPACITY;
/// Side queue capacity for [`HostObservation`] (ADR-0003 rule 4).
pub const DEFAULT_PLUGIN_SIDE_CAPACITY: usize = 128;
/// Accepted v1 default drop policy — `DropOldest` (OQ-013 closed decision point).
///
/// Per the accepted Plugin Platform RFC (2026-08-27, frontmatter `status: accepted`;
/// bitty-docs open-questions register).
pub const DEFAULT_PLUGIN_DROP_POLICY: DropPolicy = DropPolicy::DropOldest;

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

/// Full compact banner visible duration (CTX-0192).
///
/// A gated paste shows the compact one-line summary for this long, then
/// collapses to [`PASTE_BANNER_FLASH_TEXT`] while the paste still pends.
/// The flash keeps the never-silent signal without occluding the grid.
pub const PASTE_BANNER_FULL_DURATION: std::time::Duration = std::time::Duration::from_secs(4);

/// Maximum time presentation defers frames while synchronized updates
/// (`DECSET 2026`, CTX-0380) are active.
///
/// The contour/iTerm2 synchronized-output proposal leaves the abort bound
/// to the terminal; Microsoft Terminal and the ansicode reference both use
/// ~100 ms and the spec notes "a too short timeout ... won't be worse than
/// having no synchronized output at all". When the application never sends
/// the reset, the latest state still commits at this bound so a hung or
/// buggy process can never stall presentation indefinitely.
pub const SYNC_UPDATE_DEFER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Minimal status flash while a paste pends after the full banner expires
/// (CTX-0192). Bounded, single-line, always `Some` while pending.
///
/// CTX-0369: wording names the confirm gesture in plain language. Repeating
/// the paste (the chord or right-click gesture that armed the gate) is the
/// confirm action; `Esc` cancels.
pub const PASTE_BANNER_FLASH_TEXT: &str = "Paste pending (paste again to confirm, Esc cancels)";

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
    /// Leaf that owns the runtime-global primary grid (CTX-0359).
    ///
    /// The primary PTY/state path is a runtime singleton, but its grid is
    /// painted and typed into through exactly one leaf: the focused leaf at
    /// the moment the primary shell attached (initialized to the default
    /// layout's sole leaf, rebound by [`Runtime::spawn_shell_with_args`]).
    /// A session-less leaf that is not this view never paints the primary
    /// snapshot and never falls back to the primary writer.
    primary_view: Option<ViewId>,
    /// Program + args the primary shell last attached with (CTX-0359).
    ///
    /// [`Runtime::workspace_new`](Runtime::workspace_new) replays this recipe
    /// for the fresh workspace leaf so a new workspace owns a real shell from
    /// its first frame instead of staying session-less. `None` before any
    /// successful [`Runtime::spawn_shell_with_args`] (headless runtimes,
    /// startup spawn failure): the fresh leaf then stays empty and input is
    /// buffered headless, never routed to another workspace's shell.
    primary_spawn: Option<(String, Vec<String>)>,
    pending_input: Vec<u8>,
    pending_input_dropped: u64,
    renderer: GridRenderer<FallbackRasterizer<AnyRasterizer>>,
    surface: Surface,
    gpu: Option<GpuContext>,
    cold_queue: ColdQueue,
    plugin_host: PluginHost,
    last_presented_generation: u64,
    /// Per-leaf retained present primitives, keyed by visible [`ViewId`]
    /// (CTX-0386).
    ///
    /// The present compositor clears the surface every frame, so a leaf that
    /// produced no damage still contributes its retained complete draw list.
    /// A leaf re-renders only when its own origin's damage ring advanced;
    /// per-origin generation consumption lives on the primary scalar above
    /// and on `PaneSession::last_presented_generation`. Invalidated
    /// wholesale by any full-frame invalidation and by a glyph-atlas
    /// exhaustion reset. Entries are pruned to the visible allocation set
    /// after every presented frame. See `runtime::present`.
    presented_leaf_frames: std::collections::BTreeMap<ViewId, self::present::PresentedLeaf>,
    pending_full_redraw: bool,
    /// Last presented View frames (CTX-0228, decoration-aware CTX-0294).
    ///
    /// Geometry-only layout changes (tree edits via `layout_mut`,
    /// `reflow_layout`, split/zoom, decoration config) must force a full
    /// present even when no PTY bytes advanced the generation. `tick`
    /// compares the current decorated physical frames against this snapshot;
    /// any difference forces the full per-leaf path. Updated on every present
    /// (and on empty-layout idle so a frameless tree does not spin).
    last_presented_allocations: Vec<layout_focus::PresentFrame>,
    /// Focused view at the last present (CTX-0228).
    ///
    /// Cursor/focus moves change which pane paints the cursor even when
    /// allocations and generations are identical, so a focus change also
    /// forces a full present. Updated alongside the allocations.
    last_presented_focus: Option<ViewId>,
    cols: usize,
    rows: usize,
    layout: LayoutNode,
    focus: Focus,
    /// Named workspace slots (CTX-0257 entry): stashed layout + focus per
    /// workspace; the live `layout`/`focus` above mirror the active slot.
    /// At least one slot always exists; see `runtime::workspaces`.
    workspaces: Vec<WorkspaceSlot>,
    /// Active workspace index into `workspaces` (display is 1-based).
    active_workspace: usize,
    /// MRU workspace indices, active fronted, each live index exactly once.
    workspace_mru: std::collections::VecDeque<usize>,
    /// Pending per-pane session restores (CTX-0393): scrollback + cwd for
    /// session-less leaves, drained into the fresh grid by the next
    /// successful spawn of each leaf. Keyed by [`ViewId`]; bounded by the
    /// session decode caps. Never logged (may hold sensitive text).
    session_pending: BTreeMap<ViewId, session::PendingPaneRestore>,
    /// Pending kill-confirm close arm, if any (never silent kill).
    pending_ws_close: Option<PendingWsClose>,
    /// Whether the help popup (CTX-0265) is currently shown.
    ///
    /// Presentation-only overlay state: toggled by the `toggle_help`
    /// chrome action, dismissed by `Esc` or the same chord. Never grid
    /// truth; see `runtime::help`.
    help_visible: bool,
    /// Help popup rows (CTX-0265), regenerated from the live keymap
    /// registry by the app on every show.
    ///
    /// Plain display strings (`"alt+h  goto_split:left"`); bounded by
    /// [`help::HELP_MAX_ROWS`]. Painted only while `help_visible`; the
    /// paint truncates to the panel with a `+N more` tail.
    help_rows: Vec<String>,
    /// Next workspace creation sequence (display names `ws{seq}`).
    next_workspace_seq: u64,
    container: UiRect,
    clipboard: Clipboard,
    selection: Option<Selection>,
    selection_dragging: bool,
    /// Bounded multi-click tracker for word/line selection (CTX-0385).
    ///
    /// `O(1)` state (last press time, cell, button, count); the runtime is
    /// the authority — the wire [`bitty_platform::MouseEvent::click_count`]
    /// is advisory and recomputed here from press time and cell.
    click_tracker: ClickTracker,
    /// Raw press cell that started the current selection drag (CTX-0385).
    ///
    /// `O(1)`: one cell that pins word/line drag extension direction
    /// (`word_drag`/`line_drag` compare the live pointer against this, not
    /// against the expanded range). `None` when no selection is active.
    selection_anchor_press: Option<CellPos>,
    /// Click count of the last left press (`1..=3`, CTX-0385).
    ///
    /// Extension point for copy-mode (CTX-0384) and search UI (CTX-0383):
    /// they read the gesture kind via [`crate::Runtime::selection_kind`]
    /// and this count without touching the tracker.
    last_click_count: u8,
    /// Keyboard copy-mode state (CTX-0384, issue #640).
    ///
    /// `None` in normal operation; `Some` while the vi-style modal copy
    /// cursor owns keyboard input (no PTY bytes, visual selection via the
    /// CTX-0385 `SelectionKind` seams, yank to clipboard plus primary).
    /// Bounded `O(1)` state; see `runtime::copy_mode`.
    copy_mode: Option<crate::runtime::copy_mode::CopyModeState>,
    /// Active overlay-scrollbar thumb drag (CTX-0181).
    ///
    /// Press+move on the painted thumb scrolls the focused view through the
    /// existing [`View::scroll_by`] (no scroll-semantics change). `None`
    /// when no drag is active; cleared on release and when the cursor leaves
    /// the window. Presentation-only: never grid truth.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Whether the cursor has left the window since the last motion event
    /// (CTX-0181 `auto` disengagement).
    ///
    /// Set on `CursorLeft`, cleared on the next `CursorMoved`. Tracked
    /// separately from the shared `last_cursor` so the selection/mouse path
    /// keeps its meaning while the auto-hide thumb provably disengages.
    scrollbar_cursor_left: bool,
    /// Whether the scrollbar painted on the last present (CTX-0181).
    ///
    /// Compared on cursor moves so `auto` hover/proximity transitions
    /// repaint exactly once instead of presenting on every motion event.
    scrollbar_visible: bool,
    /// Active Alt+drag floating-pane move (CTX-0260).
    ///
    /// Alt+Left-press on a floating overlay grabs it; motion offsets the
    /// overlay bounds (tiled layouts have no movable position, so the grab
    /// is a fail-soft no-op there and the press falls through to
    /// selection). `None` when no drag is active; cleared on release and
    /// when the cursor leaves the window. Presentation-only: never grid
    /// truth. Shift still forces the selection path (the grab never starts
    /// while Shift is held, per the CTX-0181 precedent).
    alt_drag: Option<AltDragState>,
    /// Pending dwell before hover activation moves focus (CTX-0334).
    ///
    /// `Some` only while `mouse.focus_follows_mouse` is enabled with a
    /// positive delay and the pointer has entered a non-focused pane; the
    /// entry time is compared against the deadline on each `tick_at` (the
    /// app schedules a wake at the deadline via `EventContext::set_wait_until`).
    /// A pointer that leaves the pane or a matching focus clears it, so a
    /// transient pass-through never steals focus. Presentation-only: never
    /// grid truth.
    hover_pending: Option<HoverPending>,
    /// Renderer-side panel animation tracker (RFC-0002, CTX-0341).
    ///
    /// Presentation-only chrome transitions (open/close/focus/workspace) with
    /// bounded durations and easings. Never grid truth; frame-on-demand is
    /// preserved because `is_active`/`next_deadline` gate the present path and
    /// a completed animation schedules no further wakeups. See
    /// [`crate::runtime::animations`].
    animator: PanelAnimator,
    /// Frames retained for an in-flight close transition (RFC-0002).
    ///
    /// A removed `View` has no allocation to paint from, so its last presented
    /// frame is retained and painted as a fading ring/background until the
    /// close duration elapses. Bounded by [`MAX_CONCURRENT_ANIMATIONS`] and
    /// dropped as soon as its close animation completes.
    closing_frames: Vec<ClosingFrame>,
    /// Active workspace at the last present (RFC-0002 workspace transition
    /// detection). Mirrors `last_presented_focus` for the workspace kind.
    last_presented_workspace: usize,
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
    /// Pending view/window close confirmation (CTX-0370).
    ///
    /// The gate arm for the `close_confirm` key: `view_close_request` /
    /// `window_close_request` arm it when the mode and PTY foreground-job
    /// state require confirmation; repeating the same close gesture confirms
    /// and `Esc` cancels (`cancel_pending_on_escape`). Presentation is the
    /// shared overlay pill in the present path, never grid truth. At most
    /// one arm exists at a time.
    pending_close_confirm: Option<PendingCloseConfirm>,
    osc_clipboard_read_allowed: bool,
    osc_clipboard_write_allowed: bool,
    /// Whether OSC 10/11 dynamic color sets are allowed (CTX-0381).
    ///
    /// Capability-gated like OSC 52 writes: default `false` means untrusted
    /// PTY output cannot repaint the default fg/bg; an embedder must grant
    /// it explicitly via [`Runtime::set_osc_color_set_allowed`]. Queries are
    /// unaffected (read-only, answered from the resolved palette).
    osc_color_set_allowed: bool,
    /// Dynamic foreground override from an authorized `OSC 10` set
    /// (CTX-0381); `None` = the resolved theme foreground.
    dynamic_foreground: Option<[u8; 3]>,
    /// Dynamic background override from an authorized `OSC 11` set
    /// (CTX-0381); `None` = the resolved theme background.
    dynamic_background: Option<[u8; 3]>,
    /// When the active synchronized-update deferral window began (CTX-0380).
    ///
    /// `Some` while `DECSET 2026` is active; bounded by
    /// [`SYNC_UPDATE_DEFER_TIMEOUT`] so the window always eventually
    /// commits, and reset to `None` when the mode exits.
    sync_defer_since: Option<std::time::Instant>,
    /// Count of OSC 52 writes rejected for invalid base64 (CTX-0212).
    ///
    /// Fail-closed telemetry: the clipboard is left unchanged and a loud
    /// `eprintln!` warn fires per rejection, while this monotonic counter
    /// (wrapping) lets headless tests and the embedder observe the event
    /// without touching the real clipboard.
    osc52_rejected_writes: u64,
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
    /// Focused caret rect + preedit clip budget, refreshed each presented
    /// frame (CTX-0367). `None` when no focused cursor was painted.
    ime_caret: Option<ImeCaret>,
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
    /// Startup font size in points (CTX-0263 font zoom reset baseline).
    ///
    /// Per-window: cloned from the validated config at construction, never
    /// written back to the config file. `reset_zoom` restores this value.
    base_font_size: f32,
    /// Tail of previously seen PTY bytes retained so a terminal query split
    /// over two PTY reads is still recognized (CTX-0146). Bounded by
    /// [`crate::queries::QUERY_OVERLAP_MAX`]; raw query scans never retain
    /// more.
    query_overlap: Vec<u8>,
    /// Bounded ring of the last input events for screenshots-free debugging
    /// (CTX-0159, Issue #258). Published read-only to the `BITTY_SOCKET`
    /// introspection store; never affects terminal truth or PTY bytes.
    inspect_ring: crate::inspect::InputRing,
    /// Stored Kitty images plus cursor-anchored placements (CTX-0248).
    ///
    /// Presentation-only: composited topmost in the tick overlay path,
    /// never grid truth. Cleared per origin on alternate-screen entry
    /// (CTX-0254).
    kitty_images: bitty_rich::KittyImageLayer,
    /// Origin token of the PTY stream currently being drained (CTX-0254).
    ///
    /// `None` is the primary grid; `Some(view.0)` is the split-pane
    /// session swapped into the primary slots by `handle_pane_bytes`.
    /// Read by `kitty_display_image` to tag placements with their
    /// emitting pane, so the present layer confines each image to its
    /// own leaf (cross-pane spoof prevention). Always restored after
    /// the pane pump; never observed outside the drain path.
    kitty_origin: Option<u64>,
    /// Scaled-blit cache across present frames (CTX-0252 F2).
    ///
    /// Keyed by placement + image identity, destination rect, source dims,
    /// scrollback sequence, and geometry; scroll/geometry changes miss
    /// instead of painting stale pixels. Cleared with the image layer on
    /// alternate-screen entry.
    kitty_raster_cache: bitty_rich::KittyRasterCache,
    /// Image blits composited on the last presented frame (CTX-0252 F2).
    ///
    /// Latched on every successful present; idle ticks leave it unchanged.
    /// Bound by [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`].
    kitty_last_frame_images: usize,
    /// Alternate-screen state at the last present (CTX-0248).
    ///
    /// A change forces a full present even when the grid generation is
    /// unchanged, so entering alt clears painted images (and leaving alt
    /// repaints the restored grid) instead of idling on a stale frame.
    kitty_alt_screen_latched: bool,
    /// Decoded per-`View` background images (CTX-0347, RFC-0001/OQ-042).
    ///
    /// Loaded eagerly at construction from the validated config (global
    /// `decoration.background_image` plus every `views.*.background_image`),
    /// keyed by canonical path plus content identity. A load failure rejects
    /// the whole config; `--safe` leaves the store empty and never opens a
    /// file. The store owns the BG-4/BG-5 pool and the approved-root policy.
    backgrounds: bitty_rich::BackgroundStore,
    /// Configured path spelling -> resolved cache key (CTX-0347). The
    /// present path looks up by the same spelling the config carries, so no
    /// filesystem work happens per frame.
    background_keys: std::collections::HashMap<String, bitty_rich::BackgroundKey>,
    /// Scaled-background-blit cache across present frames (CTX-0347),
    /// keyed by source identity + fit + destination rect + DPI.
    background_rasters: bitty_rich::BackgroundRasterCache,
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
            .field("workspace_count", &self.workspaces.len())
            .field("active_workspace", &self.active_workspace)
            .field("session_pending", &self.session_pending.len())
            .field("has_pending_ws_close", &self.pending_ws_close.is_some())
            .field("help_visible", &self.help_visible)
            .field("help_rows", &self.help_rows.len())
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
    /// point per the accepted Plugin Platform RFC 2026-08-27, frontmatter
    /// `status: accepted`; bitty-docs open-questions register),
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
    /// is the accepted v1 default (OQ-013 closed decision point per the accepted Plugin Platform RFC
    /// 2026-08-27 and RFC § “Delivery, ordering, batching, and coalescing” point 3;
    /// frontmatter `status: accepted`; bitty-docs open-questions register).
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
        // CTX-0223: the headless surface spans the window (grid pixels plus
        // the padding inset on every side); tick translates grid content by
        // the inset origin and the padding band keeps the clear color.
        let extent = config.window_extent();
        let surface = Surface::headless(extent).map_err(RuntimeError::from)?;
        let cell = CellMetrics::new(config.cell_width, config.cell_height)
            .expect("validated config guarantees non-zero cell metrics");
        let query = FontQuery {
            family: config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: config.font_size,
        };
        // Vertical slice: prefer crossfont when available, fallback to headless
        // for CI determinism. Both are bounded and headless-testable. The
        // crossfont backend is wrapped in the per-glyph fallback chain
        // (CTX-0163: braille `U+2800-U+28FF` + blocks `U+2580-U+259F`;
        // CTX-0368: symbols/emoji through the pinned platform tail, with the
        // RFC tofu box painted when no loaded face covers the scalar). On
        // Windows the monospace family may be absent, so a FontNotFound from
        // GridRenderer re-tries deterministically with HeadlessRasterizer
        // instead of failing with_defaults on headless CI.
        let (renderer, is_crossfont) = {
            let base = AnyRasterizer::try_crossfont();
            let is_cf = base.is_crossfont();
            let raster = FallbackRasterizer::with_default_chain(base);
            match GridRenderer::new(raster, &query, cell) {
                Ok(r) => (r, is_cf),
                Err(err) if is_cf && matches!(&err, RenderError::FontNotFound(_)) => {
                    let fallback = FallbackRasterizer::with_default_chain(AnyRasterizer::Headless(
                        HeadlessRasterizer::new(),
                    ));
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
        let mut runtime = Self {
            cols,
            rows,
            config: config.clone(),
            parser: Parser::new(),
            state: State::with_scrollback_lines(config.scrollback),
            pty: None,
            pty_reader: None,
            pty_forward_rx: None,
            pty_forward_handle: None,
            pty_waker: None,
            pty_writer: None,
            pane_sessions: BTreeMap::new(),
            primary_view: Some(ViewId::new(1)),
            primary_spawn: None,
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            gpu: None,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            presented_leaf_frames: std::collections::BTreeMap::new(),
            pending_full_redraw: true,
            last_presented_allocations: Vec::new(),
            last_presented_focus: None,
            layout,
            focus,
            container,
            clipboard: Clipboard::new(),
            selection: None,
            selection_dragging: false,
            click_tracker: ClickTracker::new(),
            selection_anchor_press: None,
            last_click_count: crate::runtime::click::CLICK_COUNT_MIN,
            copy_mode: None,
            scrollbar_drag: None,
            scrollbar_cursor_left: false,
            scrollbar_visible: false,
            alt_drag: None,
            hover_pending: None,
            animator: PanelAnimator::new(config.animations),
            closing_frames: Vec::new(),
            last_presented_workspace: 0,
            last_clipboard_error: None,
            last_cursor: None,
            search_state: SearchState::new(),
            pending_paste: None,
            pending_paste_since: None,
            pending_close_confirm: None,
            paste_banner_collapsed: false,
            osc_clipboard_read_allowed: false,
            osc_clipboard_write_allowed: false,
            osc_color_set_allowed: false,
            dynamic_foreground: None,
            dynamic_background: None,
            sync_defer_since: None,
            osc52_rejected_writes: 0,
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
            ime_caret: None,
            wheel_accum_y: 0.0,
            wheel_accum_x: 0.0,
            wheel_line_accum_y: 0.0,
            wheel_line_accum_x: 0.0,
            scale_factor: ScaleFactor::ONE,
            is_crossfont,
            base_font_size: config.font_size,
            query_overlap: Vec::new(),
            inspect_ring: crate::inspect::InputRing::new(),
            kitty_images: bitty_rich::KittyImageLayer::new(),
            kitty_origin: None,
            kitty_raster_cache: bitty_rich::KittyRasterCache::new(),
            kitty_last_frame_images: 0,
            kitty_alt_screen_latched: false,
            backgrounds: bitty_rich::BackgroundStore::deny_all(),
            background_keys: std::collections::HashMap::new(),
            background_rasters: bitty_rich::BackgroundRasterCache::new(),
            workspaces: Vec::new(),
            active_workspace: 0,
            workspace_mru: std::collections::VecDeque::new(),
            session_pending: BTreeMap::new(),
            pending_ws_close: None,
            help_visible: false,
            help_rows: Vec::new(),
            next_workspace_seq: 2,
        };
        // CTX-0355: install the resolved palette on both the renderer (cell
        // defaults, ANSI, emitted fills) and the surface (clear color).
        runtime.renderer.set_theme_palette(config.theme);
        runtime.surface.set_theme_palette(config.theme);
        runtime.init_workspaces();
        runtime.reload_backgrounds()?;
        Ok(runtime)
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
        // CTX-0223: window-sized surface (see `with_plugin_host_capacity`).
        let extent = config.window_extent();
        let surface = Surface::headless(extent).map_err(RuntimeError::from)?;
        let cell = CellMetrics::new(config.cell_width, config.cell_height)
            .expect("validated config guarantees non-zero cell metrics");
        let query = FontQuery {
            family: config.font_family.clone(),
            style: FontStyle::Normal,
            point_size: config.font_size,
        };
        let (renderer, is_crossfont) = {
            let base = AnyRasterizer::try_crossfont();
            let is_cf = base.is_crossfont();
            let raster = FallbackRasterizer::with_default_chain(base);
            match GridRenderer::new(raster, &query, cell) {
                Ok(r) => (r, is_cf),
                Err(err) if is_cf && matches!(&err, RenderError::FontNotFound(_)) => {
                    let fallback = FallbackRasterizer::with_default_chain(AnyRasterizer::Headless(
                        HeadlessRasterizer::new(),
                    ));
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
        let mut runtime = Self {
            cols,
            rows,
            config: config.clone(),
            parser: Parser::new(),
            state: State::with_scrollback_lines(config.scrollback),
            pty: None,
            pty_reader: None,
            pty_forward_rx: None,
            pty_forward_handle: None,
            pty_waker: None,
            pty_writer: None,
            pane_sessions: BTreeMap::new(),
            primary_view: Some(ViewId::new(1)),
            primary_spawn: None,
            pending_input: Vec::new(),
            pending_input_dropped: 0,
            renderer,
            surface,
            gpu: None,
            cold_queue: ColdQueue::new(config.cold_queue_capacity),
            plugin_host,
            last_presented_generation: u64::MAX,
            presented_leaf_frames: std::collections::BTreeMap::new(),
            pending_full_redraw: true,
            last_presented_allocations: Vec::new(),
            last_presented_focus: None,
            layout,
            focus,
            container,
            clipboard: Clipboard::new(),
            selection: None,
            selection_dragging: false,
            click_tracker: ClickTracker::new(),
            selection_anchor_press: None,
            last_click_count: crate::runtime::click::CLICK_COUNT_MIN,
            copy_mode: None,
            scrollbar_drag: None,
            scrollbar_cursor_left: false,
            scrollbar_visible: false,
            alt_drag: None,
            hover_pending: None,
            animator: PanelAnimator::new(config.animations),
            closing_frames: Vec::new(),
            last_presented_workspace: 0,
            last_clipboard_error: None,
            last_cursor: None,
            search_state: SearchState::new(),
            pending_paste: None,
            pending_paste_since: None,
            pending_close_confirm: None,
            paste_banner_collapsed: false,
            osc_clipboard_read_allowed: false,
            osc_clipboard_write_allowed: false,
            osc_color_set_allowed: false,
            dynamic_foreground: None,
            dynamic_background: None,
            sync_defer_since: None,
            osc52_rejected_writes: 0,
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
            ime_caret: None,
            wheel_accum_y: 0.0,
            wheel_accum_x: 0.0,
            wheel_line_accum_y: 0.0,
            wheel_line_accum_x: 0.0,
            scale_factor: ScaleFactor::ONE,
            is_crossfont,
            base_font_size: config.font_size,
            query_overlap: Vec::new(),
            inspect_ring: crate::inspect::InputRing::new(),
            kitty_images: bitty_rich::KittyImageLayer::new(),
            kitty_origin: None,
            kitty_raster_cache: bitty_rich::KittyRasterCache::new(),
            kitty_last_frame_images: 0,
            kitty_alt_screen_latched: false,
            backgrounds: bitty_rich::BackgroundStore::deny_all(),
            background_keys: std::collections::HashMap::new(),
            background_rasters: bitty_rich::BackgroundRasterCache::new(),
            workspaces: Vec::new(),
            active_workspace: 0,
            workspace_mru: std::collections::VecDeque::new(),
            session_pending: BTreeMap::new(),
            pending_ws_close: None,
            help_visible: false,
            help_rows: Vec::new(),
            next_workspace_seq: 2,
        };
        // CTX-0355: install the resolved palette on both the renderer (cell
        // defaults, ANSI, emitted fills) and the surface (clear color).
        runtime.renderer.set_theme_palette(config.theme);
        runtime.surface.set_theme_palette(config.theme);
        runtime.init_workspaces();
        runtime.reload_backgrounds()?;
        Ok(runtime)
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
        // CTX-0355: the app constructs the real-GPU surface; install the
        // resolved palette so its clear color matches the renderer.
        surface.set_theme_palette(self.config.theme);
        self.surface = surface;
        self.gpu = Some(gpu);
        self.pending_full_redraw = true;
    }

    /// Detaches GPU, falling back to headless surface at current extent.
    pub fn detach_gpu(&mut self) {
        if let Some(extent) = self.surface.extent() {
            if let Ok(s) = Surface::headless(extent) {
                s.set_theme_palette(self.config.theme);
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

    /// Leaf that owns the runtime-global primary grid (CTX-0359).
    ///
    /// Rebound when the primary shell attaches
    /// ([`Runtime::spawn_shell_with_args`]) and when an explicit close
    /// removes the owner ([`Runtime::set_layout_closing`]). Temporary
    /// layouts that merely exclude the owner (zoom onto another leaf,
    /// workspace installs) leave it untouched, so zoom round-trips restore
    /// the same owner. The owner paints the primary snapshot and is the
    /// only session-less leaf whose input may fall back to the primary
    /// writer.
    #[must_use]
    pub fn primary_view(&self) -> Option<ViewId> {
        self.primary_view
    }

    /// Whether `view` owns the runtime-global primary grid (CTX-0359).
    #[must_use]
    pub fn is_primary_view(&self, view: &ViewId) -> bool {
        self.primary_view == Some(*view)
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

    // ------------------------------------------------------------------
    // M1 protocol state (CTX-0380 synchronized updates, CTX-0381 dynamic colors)
    // ------------------------------------------------------------------

    /// Whether any visible grid currently has synchronized updates active
    /// (`DECSET 2026`, CTX-0380).
    ///
    /// The primary state and every split-pane session own independent mode
    /// registers, so the presentation path checks all of them: a pane that
    /// enabled the mode defers its own atomic redraw exactly like the
    /// primary grid. Bounding the deferral window is the present path's job
    /// (see [`SYNC_UPDATE_DEFER_TIMEOUT`]).
    #[must_use]
    pub fn synchronized_update_active(&self) -> bool {
        self.state.modes().synchronized_update
            || self
                .pane_sessions
                .values()
                .any(|session| session.state.modes().synchronized_update)
    }

    /// Allows or denies `OSC 10`/`OSC 11` and `OSC 4` dynamic color sets
    /// (CTX-0381, CTX-0392).
    ///
    /// Capability-gated and default-deny, mirroring OSC 52 writes: untrusted
    /// PTY output can never repaint the default fg/bg or the 256-entry
    /// palette unless the embedder grants this explicitly. Queries are
    /// read-only and stay answered from the resolved palette either way.
    /// One flag gates both families (the accepted shared gating): `OSC 4`
    /// sets ride the same capability as `OSC 10`/`OSC 11`.
    pub fn set_osc_color_set_allowed(&mut self, allowed: bool) {
        self.osc_color_set_allowed = allowed;
    }

    /// Whether `OSC 10`/`OSC 11`/`OSC 4` sets are currently allowed
    /// (CTX-0381, CTX-0392).
    #[must_use]
    pub fn osc_color_set_allowed(&self) -> bool {
        self.osc_color_set_allowed
    }

    /// Active default foreground: dynamic `OSC 10` override or resolved
    /// theme color (CTX-0381), as opaque RGBA.
    #[must_use]
    pub fn active_foreground(&self) -> [u8; 4] {
        match self.dynamic_foreground {
            Some([r, g, b]) => [r, g, b, 0xFF],
            None => self.config.theme.foreground,
        }
    }

    /// Active default background: dynamic `OSC 11` override or resolved
    /// theme color (CTX-0381), as opaque RGBA.
    #[must_use]
    pub fn active_background(&self) -> [u8; 4] {
        match self.dynamic_background {
            Some([r, g, b]) => [r, g, b, 0xFF],
            None => self.config.theme.background,
        }
    }

    /// Applies an authorized `OSC 10`/`OSC 11` set and forces a repaint.
    ///
    /// The default fg/bg live on the renderer/surface palettes, so the
    /// override is installed there as well as retained for query replies;
    /// `pending_full_redraw` makes default-colored cells repaint on the next
    /// frame.
    pub(crate) fn apply_osc_color(&mut self, target: bitty_vt::DynamicColorTarget, rgb: [u8; 3]) {
        match target {
            bitty_vt::DynamicColorTarget::Foreground => self.dynamic_foreground = Some(rgb),
            bitty_vt::DynamicColorTarget::Background => self.dynamic_background = Some(rgb),
        }
        let palette = bitty_render::ThemePalette {
            foreground: self.active_foreground(),
            background: self.active_background(),
            ..self.config.theme
        };
        self.renderer.set_theme_palette(palette);
        self.surface.set_theme_palette(palette);
        self.pending_full_redraw = true;
    }

    /// Active palette entry for `index` (CTX-0392 OSC 4): the live override
    /// when present, else the base custom/preset ANSI / cube / ramp.
    ///
    /// Reads through the fixed 256-entry dynamic layer on the resolved
    /// theme palette, so custom palettes, presets, and OSC 4 overrides
    /// compose in one place. Used for query replies; rendering resolves the
    /// same way via `palette_rgb_in`.
    #[must_use]
    pub fn active_palette_color(&self, index: u8) -> [u8; 3] {
        bitty_render::grid::palette_rgb_in(&self.config.theme, index)
    }

    /// Applies one authorized `OSC 4` set and forces a repaint (CTX-0392).
    ///
    /// The override is installed on the resolved theme palette's fixed
    /// 256-entry dynamic layer (no growth, index-addressed) as well as on
    /// the renderer/surface palettes; `pending_full_redraw` makes
    /// indexed-colored cells repaint on the next frame. Out-of-range
    /// indices are unrepresentable (`u8`), so no bounds check can fail.
    pub(crate) fn apply_osc_palette(&mut self, index: u8, rgb: [u8; 3]) {
        self.config.theme.dynamic[index as usize] = Some(rgb);
        let palette = self.config.theme;
        self.renderer.set_theme_palette(palette);
        self.surface.set_theme_palette(palette);
        self.pending_full_redraw = true;
    }

    // ------------------------------------------------------------------
    // Keyboard input (CTX-0057) — winit → owned KeyEvent → legacy VT bytes → PTY
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Selection and clipboard (CTX-0059) — winit/arboard with headless fallback
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Scrollback search and selection persistence (CTX-0060) — headless
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Scrollback search UI integration (CTX-0061) — headless
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Plugin-host wiring (CTX-0027) — draft, headless, no window/GPU/Lua
    // ------------------------------------------------------------------

    // ── grant / command stubs (headless, no file I/O) ───────────────────

    // ── interception routing (open points, cold-path synchronous) ─────────

    // ------------------------------------------------------------------
    // Layout + Focus ownership (CTX-0023)
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Per-pane shell sessions (CTX-0176)
    // ------------------------------------------------------------------
}
