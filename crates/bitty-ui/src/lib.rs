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
//! - [`geometry`] — integer `Rect`, `Point`, `Size`, `SplitAxis`.
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

pub mod focus;
pub mod geometry;
pub mod layout;
pub mod selection;
pub mod view;

// Re-exports for ergonomic root access.
pub use focus::{Focus, FocusDirection};
pub use geometry::{Point, Rect, Size, SplitAxis};
pub use layout::{LayoutNode, clamp_ratio, split_rect};
pub use selection::{
    BufferPos, CellPos, PersistentSelection, Selection, SelectionKind, SelectionRange,
    is_word_char, snap_to_leading,
};
pub use view::{View, ViewId};
