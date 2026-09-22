//! `bitty-ui`: View, `LayoutNode`, Focus, and Selection primitives.
//!
//! This crate implements the UI role of the accepted crate graph (ADR-0003:
//! *View, `LayoutNode`, split/stack/overlay/focus/resize, selection primitives*;
//! depends only on `bitty-term-state`). No render, platform, or PTY coupling:
//! pure layout algebra, headless testable, deterministic split ratios and focus
//! traversal with wide-character-aware selection anchoring.
//!
//! # Role and dependency rule
//!
//! - **Reads `Snapshot` types only** through the public `bitty-term-state`
//!   surface (`Snapshot`, `Cell`). Never reaches into grid internals or
//!   mutates terminal state. Damage/present integration is deferred to
//!   `bitty-runtime`, which will composite the layout tree produced here.
//! - **No render/platform/pty dependencies.** The crate compiles without any
//!   display server and without `bitty-render` / `bitty-platform` / `bitty-pty`.
//! - **`#![forbid(unsafe_code)]`** and MSRV 1.85 (workspace `rust-version`).
//!
//! # What this slice provides
//!
//! - [`view::View`] — viewport over a `Snapshot` with scroll and column offset,
//!   resize and reflow helpers, and allocation origin assigned by the layout solver.
//! - [`layout::LayoutNode`] — owned layout tree with `Leaf`, `Split{axis,ratio}`,
//!   `Stack`, and `Overlay` variants. The solver [`LayoutNode::layout`] is total
//!   and deterministic: identical trees and container `Rect`s produce identical
//!   allocations (sorted, floor-based split arithmetic, no HashMap iteration).
//! - [`focus::Focus`] and [`focus::FocusDirection`] — deterministic leaf focus
//!   traversal: linear `Next`/`Prev` by depth-first order (wrap) and spatial
//!   `Up`/`Down`/`Left`/`Right` via rect adjacency with tie-breaking by `ViewId`.
//! - [`selection`] — `CellPos`, `SelectionRange`, `Selection` and helpers for
//!   range / word / line anchoring with wide-char awareness (spacer snapping,
//!   never splitting a `width==2` pair), plus `selected_text`.
//! - [`search`] — [`search::SearchState`] headless search UI: bounded query
//!   (`256` bytes) and matches (`1000`), deterministic `next`/`prev` with wrap,
//!   view-aware highlight mapping, `PersistentSelection` conversion, and
//!   scroll-to-current for the viewport.
//! - [`geometry`] — integer `Rect`, `Point`, `Size`, `SplitAxis`.
//! - Beacon family (U-8, CTX-0661): [`beacon_target::TargetRef`] generation
//!   handles (`Panel`/`Workspace`/`CommandBlock`/`UiNode`/`Link`, stale
//!   fails closed, no `ViewId`); [`beacon_label::LabelAllocator`] home-row
//!   first with two-char overflow and spatial left/right pools over a Lua
//!   charset policy; [`beacon_layer::BeaconAnnotationLayer`] single batched
//!   layer; [`beacon_dispatch::BeaconDispatcher`] label-to-typed-command
//!   bridge that executes nothing.
//! - [`scratchpad::ScratchpadSlot`] — single hidden per-window scratchpad
//!   slot with anchor-preserving hide/show/toggle (CW-10), routed as
//!   `bitty.workspace:scratchpad-toggle`.
//! - [`drag::DragHistory`] — bounded (`32`, `DropOldest`) undo history for
//!   drag/resize sessions (CW-09). The PW drag family builds on it (CTX-0663):
//!   [`drag::DragMoveSession`] (UX-01 Mod+drag move, `bitty.workspace:drag-move`),
//!   [`drag::apply_tiled_resize`] / [`drag::resize_floating_rect`] /
//!   [`drag::detect_resize_edge`] (UX-02 edge/corner resize,
//!   `bitty.workspace:drag-resize`), and [`drag::move_leaf_to_workspace`]
//!   (UX-09 drag-across-workspace, `bitty.workspace:drop`).
//! - [`presentation::PresentationMode`] — per-leaf display mode unifying the
//!   zoom/overlay/visibility special-cases toward one mode field (CTX-0276,
//!   CW-08): `can_transition` is the single gate for all transitions and
//!   `apply_presentation_command` routes mode changes through the workspace
//!   command registry; deliberately distinct from `Visibility` (computed
//!   display state in `bitty-runtime`).
//! - [`uitree`] — retained declarative `UiTree` with Level-1 primitives,
//!   stable `UiNodeId` diffing identities, revision-gated paint scheduling,
//!   and the `A11yRole` mapping (UX-13/UX-14, CTX-0662, candidate).
//! - [`workspace_scene`] — four-layer `WorkspaceScene` -> `View` ->
//!   `Panel` -> `Activity` spatial/identity model with the
//!   `PanelId != ViewId != TerminalId` inequality by construction
//!   (UX-15, CTX-0662, candidate).
//! - [`presentation::toggle_floating`] /
//!   [`presentation::apply_floating_toggle`] toggle one leaf Tiled ⇄ Floating
//!   (UX-03, `bitty.workspace:floating-toggle`).
//! - [`ui_levels`] — U-3 five-level architecture contract (UX-16,
//!   CTX-0668, candidate): L0 Rust mechanisms -> L1 Lua primitives ->
//!   L2 core -> L3 domain -> L4 apps, with the downward-only dependency
//!   flow rule. Governance and versioning are open.
//! - [`widget_mech`] — U-3 complex-widget mechanism split (UX-17,
//!   CTX-0668, candidate): Rust-owned virtualization windows, IME
//!   composition state, scroll offsets, and canvas command budgets keyed
//!   by the canonical [`uitree::UiNodeId`]; appearance stays Lua-side.
//!   Headless only: no render, exec, or plugin coupling (beacon-style
//!   plugin migration recorded as a follow-up in the module docs).
//! - [`gesture::GestureTransaction`] — U-5 gesture transaction
//!   (lift/preview/commit/Esc-rollback, interactive drop targets, atomic
//!   registry commit) and [`gesture::CommandOrigin`] ontological
//!   equivalence (gesture/keyboard/palette/CLI/IPC/agent through one
//!   registry, UX-21/UX-22).

//! - [`window_chrome`] — headless per-window chrome runtime over the five
//!   named surfaces (`WorkspaceRail`, `StatusBar`, `OverlayRoot`,
//!   `NotificationArea`, `CommandSurface`) with a bounded notice queue and
//!   a single command session (UX-18, CTX-0669, candidate).
//! - [`panel_rules`] — typed panel rules (placement, presentation, minimum
//!   size, accent) with origin rank, selector specificity, and fail-closed
//!   conflict diagnostics (UX-19, CTX-0669, candidate).
//! - [`resolved_style`] — seven-layer `ResolvedStyle` cascade (safety above
//!   user rule above workspace rule above user theme above plugin
//!   preference above plugin content theme above framework default) with
//!   per-key attribution (UX-20, CTX-0669, candidate).

//! - [`canvas`] — bounded `Canvas` display lists retained for compositor
//!   replay at refresh, revision-gated like [`uitree`] (UX-25, CTX-0671,
//!   candidate).
//! - [`budget`] — UI/GPU resource budget tiers (node count, texture
//!   memory, blur area, draw calls) with refuse-vs-degrade admission
//!   (UX-24, CTX-0671, candidate).
//! - [`motion`] — Core-owned motion hierarchy (`motion.default` ->
//!   `panel` -> `panel.open/move/close`): Lua sets targets, Rust
//!   interpolates, reduced motion is mandatory, zero wakeups at rest
//!   (UX-23, CTX-0671, candidate).

//!
//! # Determinism
//!
//! All layout and selection primitives are pure functions of their inputs: no
//! wall-clock time, randomness, or platform variance participates. Split ratios
//! are clamped to `[0.10, 0.90]` (or `0.5` for non-finite) and applied with
//! `floor` and clamping to exact integer partitioning so container widths/heights
//! are covered without gaps. Focus adjacency picks maximal overlap, then
//! smallest `ViewId`. Selection snapping maps spacer columns to their leading
//! halves.
//!
//! # Headless friendliness
//!
//! Every operation is exercised headlessly in unit tests. No window system is
//! required.

#![forbid(unsafe_code)]

pub mod a11y;
pub mod beacon_dispatch;
pub mod beacon_label;
pub mod beacon_layer;
pub mod beacon_target;
pub mod budget;
pub mod canvas;
pub mod decoration;
pub mod drag;
pub mod focus;
pub mod geometry;
pub mod gesture;
pub mod layout;
pub mod motion;
pub mod panel;
pub mod panel_rules;
pub mod presentation;
pub mod provider;
pub mod resolved_style;
pub mod scratchpad;
pub mod scrollbar;
pub mod search;
pub mod selection;
pub mod theme;
pub mod ui_levels;
pub mod uitree;
pub mod view;
pub mod widget_mech;
pub mod window_chrome;

pub mod workspace_scene;

// Re-exports for ergonomic root access.
pub use beacon_dispatch::{BeaconDispatcher, DispatchError, MAX_BEACON_BINDINGS};
pub use beacon_label::{
    DEFAULT_HOME_CHARSET, LabelAllocator, LabelError, LabelPolicy, MAX_BEACON_TARGETS,
    MAX_CHARSET_LEN,
};
pub use beacon_layer::{
    AnnotationLayerError, BeaconAnnotation, BeaconAnnotationLayer, MAX_BEACON_ANNOTATIONS,
};
pub use beacon_target::{
    CommandBlockId, CommandBlockRef, LinkId, LinkRef, MAX_TARGETS_PER_KIND, PanelRef, TargetError,
    TargetRef, TargetRegistry, UiNodeRef, WorkspaceId, WorkspaceRef,
};
pub use budget::{
    Admission, BudgetDimension, BudgetTier, ESSENTIAL_TEXTURE_BYTES, Overrun, RICH_TEXTURE_BYTES,
    ResourceBudget, ResourceUsage, STANDARD_TEXTURE_BYTES, tree_nodes,
};
pub use canvas::{
    CanvasCommand, CanvasDisplayList, CanvasError, CanvasLayer, CanvasSubmitReport,
    MAX_CANVAS_COMMANDS, MAX_CANVAS_COORD, MAX_CANVAS_RADIUS, MAX_CANVAS_SURFACES,
    MAX_CANVAS_TEXT_LEN,
};
pub use decoration::{
    DEFAULT_BORDER_PX, DEFAULT_CONTENT_INSET_PX, DEFAULT_GAPS_IN_PX, DEFAULT_GAPS_OUT_PX,
    DEFAULT_RADIUS_PX, DecoratedView, Decoration, DecorationError, MAX_BORDER_PX,
    MAX_CONTENT_INSET_PX, MAX_GAP_PX, MAX_RADIUS_PX,
};
pub use drag::{
    CrossWorkspaceDrop, CrossWorkspaceError, DRAG_HISTORY_CAP, DRAG_MOVE_CMD, DRAG_RESIZE_CMD,
    DragHistory, DragMoveError, DragMoveSession, DragResizeError, DropSpec,
    MAX_VIEWS_PER_WORKSPACE_TREE, RESIZE_HANDLE_CELLS, ResizeEdge, WORKSPACE_DROP_CMD,
    apply_tiled_resize, detect_resize_edge, move_leaf_to_workspace, resize_floating_rect,
    workspace_drop_target,
};
pub use focus::{Focus, FocusDirection};
pub use geometry::{Gaps, Point, Rect, Size, SplitAxis};
pub use gesture::{
    CommandInvocation, CommandOrigin, DropTarget, EquivalenceError, GestureError, GestureOutcome,
    GesturePhase, GestureTransaction, resolve_invocation, verify_origin_equivalence,
};
pub use layout::{
    LayoutNode, OverlayLayer, OverlayTier, clamp_ratio, smart_split_axis, split_rect,
    split_rect_with_gap,
};
pub use motion::{
    MAX_MOTION_DURATION_MS, MotionConfig, MotionCurve, MotionError, MotionScope, MotionSpec,
    MotionValue,
};
pub use panel::{
    BrowserSurfaceId, CommandError, CommandRegistry, InputTarget, MAX_COMMANDS_PER_PANEL_TYPE,
    MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN, MAX_OVERLAYS_PER_WINDOW, Overlay, OverlayError,
    OverlayKind, OverlayManager, PanelFocus, PanelId, PanelState, PanelType, QualifiedCommand,
    ViewContent, route_input, validate_panel_bounds,
};
pub use panel_rules::{
    EffectKind, MAX_PANEL_RULES, PanelRule, PanelRuleSet, RuleDiagnostic, RuleEffect, RuleError,
    RuleId, RuleOrigin, RuleSelector,
};
pub use presentation::{
    FLOATING_CMD_TOGGLE, FloatingToggleError, PRESENTATION_CMD_FLOATING,
    PRESENTATION_CMD_FULLSCREEN, PRESENTATION_CMD_SCRATCHPAD, PRESENTATION_CMD_TILED,
    PresentationCommandError, PresentationMode, apply_floating_toggle, apply_presentation_command,
    presentation_command_for_mode, presentation_mode_for_command,
};
pub use provider::{
    DEFAULT_PROVIDER_RATIO, DWINDLE_PROVIDER_ID, DwindleProvider, GRID_PROVIDER_ID, GridProvider,
    LAYOUT_PROVIDER_CAPABILITY, LayoutError, LayoutProvider, LogicalRect as ProviderRect,
    MASTER_PROVIDER_ID, MAX_PROVIDER_NAME_LEN, MasterProvider, NOOP_PROVIDER_ID,
    NOOP_PROVIDER_NAME, NoopTiler, ProviderId, ProviderName, ProviderRegistry,
    RESERVED_PROVIDER_NAMES, WorkspaceSnapshot, validate_proposal,
};
pub use resolved_style::{ResolvedStyle, StyleCascade, StyleError, StyleOrigin};
pub use scratchpad::{
    HiddenEntry, SCRATCHPAD_CMD_TOGGLE, ScratchpadError, ScratchpadSlot, apply_scratchpad_toggle,
};
pub use scrollbar::{
    MIN_THUMB_HEIGHT_PX, SCROLLBAR_PROXIMITY_PX, ScrollbarHit, ScrollbarMode, ThumbSpan, TrackRect,
    TrackSpec, hit_test, is_visible, offset_for_thumb_y, thumb_geometry, track_rect,
};
pub use search::{SearchHighlight, SearchState, search_match_to_persistent};
pub use selection::{
    BufferPos, CellPos, PersistentSelection, Selection, SelectionKind, SelectionRange,
    is_word_char, snap_to_leading,
};
pub use ui_levels::{
    LevelFlowError, U3_LEVELS_CONTRACT_VERSION, UiLevel, check_flow, level_of, may_depend_on,
};
pub use uitree::{
    ApplyReport, UiChange, UiChangeKind, UiNode, UiNodeId, UiNodeKind, UiTree, UiTreeError,
    a11y_role_of, diff_trees,
};
pub use view::{View, ViewId};
pub use widget_mech::{
    CanvasMech, MAX_ITEM_HEIGHT_PX, MAX_MECH_CANVAS_COMMANDS, MAX_MECH_CANVAS_DIM_PX,
    MAX_SCROLL_CONTENT_PX, MAX_VIEWPORT_PX, MAX_VIRTUAL_ITEMS, ScrollMech, TextInputMech,
    VirtualListMech, WidgetMechError,
};
pub use window_chrome::{
    ChromeError, ChromeNotification, ChromeSurface, MAX_NOTIFICATION_TEXT_LEN, MAX_NOTIFICATIONS,
    MAX_OVERLAY_NODES, NotificationId, NotificationSeverity, WindowChromeRuntime,
};
pub use workspace_scene::{
    ActivityId, ActivityStack, LayerEntry, PanelAttachment, SceneError, SceneLayer,
    TerminalBinding, WorkspaceScene, WorkspaceSceneId,
};
