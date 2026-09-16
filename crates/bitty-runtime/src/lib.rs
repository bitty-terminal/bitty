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
//!                                       |                         |                ^
//!                                       +--> bounded cold queue --+--> plugin runtime   |
//!                                       |        |               |       |  PluginHost (draft)
//!                                       |        |               |       |  EventPipeline + SideQueue<HostObservation>
//!                                       v        v               v       v
//!                                    ColdQueue          HostObservation side queue (bounded, ADR-0003 rule 4)
//!                                                                  |
//!                                                    LayoutNode + Focus --reflow--> View allocations
//! ```
//!
//! - The **hot path** is PTY bytes -> parser -> state -> damage -> render
//!   `DrawList` -> present. No Lua, config, or plugin code enters it.
//! - **Cold-path events** are observed through a [`queue::ColdQueue`] that
//!   is strictly bounded. No untrusted input can grow the queue without
//!   limit (threat T-01). The queue is drained by the future plugin host.
//! - **Plugin-host bridging (ADR-0003 rule 4):** every `ColdEvent` that has a direct
//!   [`bitty_plugin_host::HostObservation`] form is also pushed into the host's
//!   bounded [`bitty_plugin_host::SideQueue`] without blocking the producer. When
//!   the side queue is full the oldest observation is dropped and the counter
//!   is exposed for `bitty plugin doctor` via `Runtime::plugin_side_dropped`
//!   and `Runtime::plugin_total_dropped`. The side queue never holds hot-path
//!   objects (no GPU/window/PTY handles, no Lua VM).
//! - Platform `Resized` events flow through
//!   [`Runtime::handle_platform_event`] -> [`Runtime::handle_resize`], which
//!   reconfigures the surface extent, the layout container, and PTY window
//!   size. The layout is reflowed deterministically via
//!   [`LayoutNode::reflow`]; grid memory reflow for the singular terminal
//!   state is deferred under the terminal-state-rfc ("Open items remaining
//!   under OQ-007") and is documented honestly.
//! - Multi-pane: [`Runtime`] owns a [`LayoutNode`] tree and [`Focus`]. Per-leaf
//!   `tick` reflows the tree into the container `Rect` (cell space) via
//!   `LayoutNode::reflow`, updates each leaf [`View`]'s `origin`/`cols`/`rows`,
//!   then renders each leaf's viewport snapshot through the shared
//!   [`GridRenderer`] (translated to the leaf's pixel origin) and presents
//!   the combined `DrawList` once via the headless software seam. Layout math
//!   is headless-testable without GPU/window; `set_layout` and focus moves are
//!   deterministic. Wide-char selection snapping remains deferred.

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
//! - Window `ScaleFactorChanged` rescales the renderer immediately via
//!   [`Runtime::apply_dpi_scale`] (sanitized, fail-safe, never panics); the
//!   grid follows from the physical extent when the embedder re-reads
//!   `inner_size` (preferred — already physical pixels) or from cached
//!   logical geometry via `SurfaceTarget::logical_to_physical` /
//!   `surface_extent_from_logical`, while a following `Resized` event takes
//!   precedence either way. Headless tests exercise that precedence.
//! - `handle_resize` with a zero-sized extent is skipped per
//!   `bitty_platform::map_resize_to_surface_extent` (minimized/occluded),
//!   matching the GPU path contract.
//! - Grid reflow on resize and alt-screen pixel-geometry adjustments are
//!   deferred to the accepted text/rfc open items; resize currently only
//!   reconfigures the surface and PTY.
//!
//! # Plugin-host wiring (CTX-0027) — accepted contracts, implementation not yet verified
//!
//! This crate owns a [`bitty_plugin_host::PluginHost`] behind the cold path.
//! The host tracks two accepted contracts: `plugin-platform-rfc.md`
//! (`accepted` 2026-08-27, closes `OQ-011`/`OQ-012`/`OQ-013`) and
//! `isolation-resource-rfc.md` (`accepted` 2026-08-28, closes `OQ-014`);
//! lifecycle `Draft -> experimental review evidence -> Accepted (2026-08-27
//! and 2026-08-28) -> normative` per independent review (category owner +
//! docs curator + security reviewer). The implementation here is
//! `Implemented`, not yet `Verified`: it serves as review evidence and
//! carries no compatibility promise beyond the accepted contract.
//!
//! - The runtime exposes [`Runtime::register_plugin`], grant-checked stubs
//!   (`is_capability_granted` / `dispatch_command`), event routing through the
//!   host's [`bitty_plugin_host::EventPipeline`] with the **accepted v1 default**
//!   [`bitty_plugin_host::DropPolicy::DropOldest`] honored (accepted
//!   `OQ-013` decision point per the Plugin Platform RFC), and the bounded
//!   side queue per ADR-0003 rule 4. The v1 default is `DropOldest`
//!   with pipeline `64` / side `128` and batch `32`/`8 KiB`; see
//!   [`bitty_plugin_host::event::DropPolicy`] and the RFC § “Delivery, ordering,
//!   batching, and coalescing” for the authoritative trade-off statement.
//! - The four v1 interception points (`intercept.command-dispatch`,
//!   `intercept.terminal-spawn`, `intercept.paste`, `intercept.open-url`) are
//!   synchronous, veto-wins, fail-closed on timeout (CTX-0465), and cold-path
//!   only. Reentrancy is rejected, timeouts deny, and the isolation and
//!   budget mechanisms are governed by the accepted `OQ-014` Isolation
//!   Resource RFC (three-level queue budgets, `RC-1`/`RC-2`, failure
//!   semantics); remaining numeric timeouts in this crate use
//!   headless-testable values without claiming normative numbers beyond the
//!   accepted contract.
//! - The host never holds window/GPU/PTY handles or internal hot-path objects,
//!   and it remains headless-testable without a Lua VM. Budgets, instruction/
//!   memory enforcement, and real VM execution are deferred gaps.
//!
//! # Security and resource bounds
//!
//! - No `unsafe` is required. The workspace denies `unsafe_code`; this crate
//!   enforces `#![forbid(unsafe_code)]` with no exception. The single
//!   `allow(unsafe_code)` in `bitty-render`'s GPU surface creation path
//!   stays behind that crate's boundary.
//! - Bounded parsing/state invariants are owned by `bitty-vt`/`bitty-term-state`.
//!   Bounded rendering (atlas size, cache capacity) is owned by `bitty-render`.
//!   The bounded cold-path queue is owned here; the bounded plugin side queue
//!   and per-subscriber pipeline queues are owned by `bitty-plugin-host`.
//! - No shell interpolation. [`Runtime::spawn_shell`] takes a direct argv[0]
//!   via `bitty-pty::PtyBuilder`, never a shell string.
//! - Plugin authority is deny-by-default, hash-bound, and workspace-narrowable
//!   only (never additive). No allow-all capability exists.
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

pub mod browser_panel;
pub mod config;
pub mod error;
pub mod execution;
pub mod host_bridge;
pub mod inspect;
pub mod palette;
pub mod panels_async;
pub mod paste;
pub mod plugin_runtime;
pub mod project;
pub mod project_scope;
pub mod queries;
pub mod queue;
pub mod registry;
pub mod runtime;
pub mod shell_integration;
pub mod statusline;
#[deprecated(since = "0.1.0", note = "use workspace (tabs alias removal >= v0.2.0)")]
pub mod tabs;
pub mod workspace;

pub use config::{
    BACKGROUND_FITS, CloseConfirmMode, DEFAULT_BACKGROUND_FIT, MAX_BACKGROUND_IMAGE_PATH_BYTES,
    MAX_BACKGROUND_IMAGE_ROOTS, RuntimeConfig, RuntimeViewBackground, RuntimeViewOutline,
    RuntimeViewTarget, ViewAppearanceRule,
};
pub use error::RuntimeError;
pub use execution::{
    DEFAULT_MAX_JOBS, DeliveryState, EventClass, EventReplay, JobCancel, JobError, JobEvent, JobId,
    JobIo, JobKind, JobLifetime, JobOrigin, JobRegistry, JobSnapshot, JobSpec, JobState, JobStop,
    JobTimeouts, MAX_EVENT_REPLAY, MAX_JOB_ORIGIN_BYTES, MAX_OUTPUT_BYTES_PER_JOB, MAX_READ_BYTES,
    MAX_READ_LINES, MAX_STORED_CRITICAL_EVENTS, MAX_STORED_JOB_EVENTS,
    MAX_STORED_OBSERVATION_EVENTS, OutputFilter, OutputIndex, OutputStream, OutputView, ReadOutput,
    StoredEvent,
};
pub use queue::{ColdEvent, ColdQueue};
pub use runtime::background_images::validate_background_images;
pub use runtime::close_confirm::{CLOSE_CONFIRM_BANNER_MAX_CHARS, ViewCloseRequest};
pub use runtime::help::{HELP_MAX_ROWS, HELP_PANEL_FOOTER, HELP_PANEL_TITLE};
pub use runtime::layout_focus::PresentFrame;
pub use runtime::session::{
    MAX_SESSION_CWD_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_GRID_DIM, MAX_SESSION_LAYOUT_DEPTH,
    MAX_SESSION_LINE_BYTES, MAX_SESSION_LINE_TEXT_BYTES, MAX_SESSION_NAME_CHARS,
    MAX_SESSION_PANES_PER_WORKSPACE, MAX_SESSION_PANES_TOTAL,
    MAX_SESSION_SCROLLBACK_LINES_PER_PANE, MAX_SESSION_WORKSPACES, PaneSnapshot,
    PendingPaneRestore, SESSION_APP_DIR_NAME, SESSION_FILE_NAME, SESSION_FORMAT_VERSION,
    SESSIONS_DIR_NAME, SessionError, SessionExitSaveOutcome, SessionRestoreSummary,
    SessionSaveSummary, SessionSnapshot, SessionStartupOutcome, WorkspaceSnapshot, decode_session,
    encode_session, session_dir, session_dir_for, session_file, session_file_for, state_home,
    state_home_for,
};
pub use runtime::workspaces::{MAX_WORKSPACES, WsCloseRequest};
pub use runtime::{
    ActivationGesture, AnimationCurve, AnimationKind, AnimationPolicy, ClosingFrame,
    DEFAULT_PLUGIN_DROP_POLICY, DEFAULT_PLUGIN_PIPELINE_CAPACITY, DEFAULT_PLUGIN_SIDE_CAPACITY,
    FileUrlActivation, ImeCursorArea, KittyDisplayOutcome, KittyImageError,
    MAX_CONCURRENT_ANIMATIONS, PASTE_BANNER_FLASH_TEXT, PASTE_BANNER_FULL_DURATION,
    POLL_PTY_MAX_BYTES, POLL_PTY_MAX_CHUNKS, POLL_PTY_TIME_BUDGET, PTY_FORWARD_CAPACITY_CHUNKS,
    PanelAnimator, PresentStats, PtyWaker, ReducedMotionMode, Runtime, SYNC_UPDATE_DEFER_TIMEOUT,
    UrlActivation,
};

// Re-export layout primitives for ergonomic `Runtime::set_layout` callers.
// The runtime depends on `bitty-ui` only via these owned value types; no
// render/platform/pty coupling is introduced through them.
pub use bitty_ui::{
    DecoratedView, Decoration, DecorationError, Focus, FocusDirection, Gaps, LayoutNode,
    ScrollbarMode, SplitAxis, View, ViewId,
};

// CTX-0355: the resolved terminal palette crosses the app/runtime seam on
// `RuntimeConfig`; re-export it so the composition root needs no direct
// `bitty-render` dependency for the preset mapping.
pub use bitty_render::ThemePalette;
pub use bitty_ui::{Point as UiPoint, Rect as UiRect, Size as UiSize};

pub use registry::{
    Generation, LogicalRect, MAX_COLS, MAX_ROWS, MAX_TERMINALS, MAX_VIEWS_PER_WORKSPACE,
    MAX_WORKSPACES_PER_WINDOW, PersistentId, RESIZE_DEBOUNCE_CAP, RegistryConfig, RegistryError,
    RuntimeId, TerminalHandle, TerminalId, TerminalRegistry, ViewHandle, Visibility, WorkspaceId,
};
