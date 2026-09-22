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
//! - [`scratchpad::ScratchpadSlot`] — single hidden per-window scratchpad
//!   slot with anchor-preserving hide/show/toggle (CW-10), routed as
//!   `bitty.workspace:scratchpad-toggle`.
//! - [`drag::DragHistory`] — bounded (`32`, `DropOldest`) undo history for
//!   drag/resize sessions (CW-09).
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
pub mod decoration;
pub mod drag;
pub mod focus;
pub mod geometry;
pub mod layout;
pub mod panel;
pub mod presentation;
pub mod provider;
pub mod scratchpad;
pub mod scrollbar;
pub mod search;
pub mod selection;
pub mod theme;
pub mod uitree;
pub mod view;
pub mod workspace_scene;

// Re-exports for ergonomic root access.
pub use decoration::{
    DEFAULT_BORDER_PX, DEFAULT_CONTENT_INSET_PX, DEFAULT_GAPS_IN_PX, DEFAULT_GAPS_OUT_PX,
    DEFAULT_RADIUS_PX, DecoratedView, Decoration, DecorationError, MAX_BORDER_PX,
    MAX_CONTENT_INSET_PX, MAX_GAP_PX, MAX_RADIUS_PX,
};
pub use drag::{DRAG_HISTORY_CAP, DragHistory};
pub use focus::{Focus, FocusDirection};
pub use geometry::{Gaps, Point, Rect, Size, SplitAxis};
pub use layout::{
    LayoutNode, OverlayLayer, OverlayTier, clamp_ratio, smart_split_axis, split_rect,
    split_rect_with_gap,
};
pub use panel::{
    BrowserSurfaceId, CommandError, CommandRegistry, InputTarget, MAX_COMMANDS_PER_PANEL_TYPE,
    MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN, MAX_OVERLAYS_PER_WINDOW, Overlay, OverlayError,
    OverlayKind, OverlayManager, PanelFocus, PanelId, PanelState, PanelType, QualifiedCommand,
    ViewContent, route_input, validate_panel_bounds,
};
pub use presentation::{
    PRESENTATION_CMD_FLOATING, PRESENTATION_CMD_FULLSCREEN, PRESENTATION_CMD_SCRATCHPAD,
    PRESENTATION_CMD_TILED, PresentationCommandError, PresentationMode, apply_presentation_command,
    presentation_command_for_mode, presentation_mode_for_command,
};
pub use provider::{
    DEFAULT_PROVIDER_RATIO, DWINDLE_PROVIDER_ID, DwindleProvider, GRID_PROVIDER_ID, GridProvider,
    LAYOUT_PROVIDER_CAPABILITY, LayoutError, LayoutProvider, LogicalRect as ProviderRect,
    MASTER_PROVIDER_ID, MAX_PROVIDER_NAME_LEN, MasterProvider, NOOP_PROVIDER_ID,
    NOOP_PROVIDER_NAME, NoopTiler, ProviderId, ProviderName, ProviderRegistry,
    RESERVED_PROVIDER_NAMES, WorkspaceSnapshot, validate_proposal,
};
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
pub use uitree::{
    ApplyReport, UiChange, UiChangeKind, UiNode, UiNodeId, UiNodeKind, UiTree, UiTreeError,
    a11y_role_of, diff_trees,
};
pub use view::{View, ViewId};
pub use workspace_scene::{
    ActivityId, ActivityStack, LayerEntry, PanelAttachment, SceneError, SceneLayer,
    TerminalBinding, WorkspaceScene, WorkspaceSceneId,
};
