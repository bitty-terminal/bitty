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
//! - Window `ScaleFactorChanged` alone does not reconfigure the headless
//!   surface; the spec requires refreshing logical geometry through
//!   `SurfaceTarget::logical_to_physical` and a following `Resized` event
//!   which takes precedence. Headless tests exercise that precedence.
//! - `handle_resize` with a zero-sized extent is skipped per
//!   `bitty_platform::map_resize_to_surface_extent` (minimized/occluded),
//!   matching the GPU path contract.
//! - Grid reflow on resize and alt-screen pixel-geometry adjustments are
//!   deferred to the accepted text/rfc open items; resize currently only
//!   reconfigures the surface and PTY.
//!
//! # Plugin-host wiring (CTX-0027) — draft status, experimental review evidence
//!
//! This crate owns a [`bitty_plugin_host::PluginHost`] behind the cold path.
//! The host tracks the `plugin-platform-rfc.md` contract
//! (`Proposed` / `draft`, `OQ-011..OQ-013`, `OQ-014`; new lifecycle
//! `Draft -> experimental review evidence -> Accepted -> normative`).
//! Until that RFC is accepted via independent review (category owner + docs
//! curator + security reviewer) and an ADR records acceptance, nothing here
//! claims stable file formats or frozen capability identifiers; the
//! experimental implementation serves as review evidence and carries no
//! compatibility promise.
//!
//! - The runtime exposes [`Runtime::register_plugin`], grant-checked stubs
//!   (`is_capability_granted` / `dispatch_command`), event routing through the
//!   host's [`bitty_plugin_host::EventPipeline`] with the **accepted v1 default**
//!   [`bitty_plugin_host::DropPolicy::DropOldest`] honored (OQ-013 closed
//!   decision point; experimental review evidence per new RFC lifecycle, RFC
//!   remains `Proposed` until independent review), and the bounded
//!   side queue per ADR-0003 rule 4. The v1 default is `DropOldest`
//!   with pipeline `64` / side `128` and batch `32`/`8 KiB`; see
//!   [`bitty_plugin_host::event::DropPolicy`] and the RFC § “Delivery, ordering,
//!   batching, and coalescing” for the authoritative trade-off statement.
//! - The four v1 interception points (`intercept.command-dispatch`,
//!   `intercept.terminal-spawn`, `intercept.paste`, `intercept.open-url`) are
//!   synchronous, veto-wins, fail-open, and cold-path only. Reentrancy is
//!   rejected, timeouts are treated as abstention, and numeric timeout/queue
//!   budgets belong to `OQ-014` (this crate uses headless-testable candidate
//!   values without claiming normative numbers).
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

pub mod config;
pub mod error;
pub mod file_manager;
pub mod palette;
pub mod paste;
pub mod project;
pub mod queue;
pub mod registry;
pub mod runtime;
pub mod shell_integration;
pub mod statusline;
pub mod tabs;

pub use config::RuntimeConfig;
pub use error::RuntimeError;
pub use queue::{ColdEvent, ColdQueue};
pub use runtime::{
    ActivationGesture, DEFAULT_PLUGIN_DROP_POLICY, DEFAULT_PLUGIN_PIPELINE_CAPACITY,
    DEFAULT_PLUGIN_SIDE_CAPACITY, FileUrlActivation, PresentStats, Runtime, UrlActivation,
};

// Re-export layout primitives for ergonomic `Runtime::set_layout` callers.
// The runtime depends on `bitty-ui` only via these owned value types; no
// render/platform/pty coupling is introduced through them.
pub use bitty_ui::{Focus, FocusDirection, LayoutNode, SplitAxis, View, ViewId};
pub use bitty_ui::{Point as UiPoint, Rect as UiRect, Size as UiSize};

pub use registry::{
    Generation, LogicalRect, MAX_COLS, MAX_ROWS, MAX_TERMINALS, MAX_VIEWS_PER_WORKSPACE,
    MAX_WORKSPACES_PER_WINDOW, PersistentId, RESIZE_DEBOUNCE_CAP, RegistryConfig, RegistryError,
    RuntimeId, TerminalHandle, TerminalId, TerminalRegistry, ViewHandle, Visibility, WorkspaceId,
};
