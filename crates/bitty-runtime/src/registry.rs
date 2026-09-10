//! TerminalRegistry and View lifecycle for multi-terminal Workspace (CTX-0101).
//!
//! Implements the accepted contract from
//! `terminal-registry-view-lifecycle-rfc.md` (CTX-0117, 6f30c2f):
//! strict `TerminalId != ViewId` with `RuntimeId` vs `PersistentId`,
//! per-registry generation, attach/detach, focus MRU, layout, visibility,
//! persistence, bounded 64/32/16, and typed failure semantics.
//!
//! One registry per process/window, views share `Renderer`/`GridRenderer`
//! but distinct `State`/PTY. No Panel/Browser hardcode, single-process
//! `winit` window. PTY size flows only from validated `LogicalRect` via
//! DPI-aware `floor(rect / cell)` and debounce 64. Visibility is a
//! presentation property (5 states) that never mutates grid/scrollback.
//!
//! All allocations are validated before mutation (fail-closed). No panic
//! on invalid handles; typed errors and previous valid state retained.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use bitty_term_state::{Snapshot, State};
use bitty_ui::{
    Focus, Gaps, LayoutNode, Rect as UiRect, View, ViewId,
    panel::{
        CommandRegistry as UiCommandRegistry, OverlayKind as UiOverlayKind,
        OverlayManager as UiOverlayManager, PanelState as UiPanelState, PanelType as UiPanelType,
    },
};

// ---------------------------------------------------------------------------
// Constants per RFC bounded-resource table
// ---------------------------------------------------------------------------

pub const MAX_TERMINALS: usize = 64;
pub const MAX_VIEWS_PER_WORKSPACE: usize = 32;
pub const MAX_WORKSPACES_PER_WINDOW: usize = 16;

pub const DEFAULT_MAX_TERMINALS: usize = 16;
pub const DEFAULT_MAX_VIEWS_PER_WORKSPACE: usize = 16;
pub const DEFAULT_MAX_WORKSPACES_PER_WINDOW: usize = 8;

pub const MAX_PERSISTENT_ID_LEN: usize = 64;
pub const MAX_COLS: u16 = 1024;
pub const MAX_ROWS: u16 = 1024;
pub const RESIZE_DEBOUNCE_CAP: usize = 64;
pub const GENERATION_RESERVE: u64 = 1024;

// ---------------------------------------------------------------------------
// Identity newtypes — pairwise incompatible, no From/Into bridges
// ---------------------------------------------------------------------------

/// Stable handle for a `Terminal` within one registry generation.
/// Distinct newtype from `ViewId`; no transmute or `From` bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalId(pub u64);

impl TerminalId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TerminalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TerminalId({})", self.0)
    }
}

/// Ephemeral identifier bound to a live PTY incarnation. Never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuntimeId(pub u64);

impl RuntimeId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Stable handle for a `Workspace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(pub u64);

impl WorkspaceId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Monotonic per-registry generation. Starts at 1, never 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(pub u64);

impl Generation {
    pub const INITIAL: Self = Self(1);
    pub const RESERVED_TOP: Self = Self(u64::MAX - GENERATION_RESERVE);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns next generation or `GenerationExhausted` when within reserve.
    pub fn next(self) -> Result<Self, RegistryError> {
        if self.0 >= u64::MAX - GENERATION_RESERVE {
            return Err(RegistryError::GenerationExhausted { current: self });
        }
        Ok(Self(self.0 + 1))
    }

    pub fn is_exhausted(self) -> bool {
        self.0 >= u64::MAX - GENERATION_RESERVE
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "generation({})", self.0)
    }
}

/// Optional stable identifier for a terminal that survives restarts.
/// Bounded to `<= 64` bytes, UTF-8, charset `[a-z0-9_-]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PersistentId(String);

impl PersistentId {
    /// Validates and creates a `PersistentId`.
    ///
    /// # Errors
    /// `InvalidPersistentId` when charset, length, or UTF-8 bounds violated.
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let s = value.into();
        Self::validate_str(&s)?;
        Ok(Self(s))
    }

    fn validate_str(s: &str) -> Result<(), RegistryError> {
        if s.is_empty() {
            return Err(RegistryError::InvalidPersistentId {
                reason: "persistent id must not be empty",
                value: s.to_owned(),
            });
        }
        if s.len() > MAX_PERSISTENT_ID_LEN {
            return Err(RegistryError::InvalidPersistentId {
                reason: "persistent id exceeds 64 bytes",
                value: s.to_owned(),
            });
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(RegistryError::InvalidPersistentId {
                reason: "persistent id charset must be [a-z0-9_-]",
                value: s.to_owned(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PersistentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// `ViewId` is owned by `bitty-ui`; we re-export for ergonomic `registry::ViewId`
// but keep `TerminalId != ViewId` distinctness via type-level separation.
// No `From` impls bridge them.
pub use bitty_ui::ViewId as RegistryViewId;

// ---------------------------------------------------------------------------
// Geometry: LogicalRect (validated, logical pixels)
// ---------------------------------------------------------------------------

/// Validated rectangle in logical pixels produced by the Workspace
/// compositor. Converted to PTY grid via DPI-aware cell metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl LogicalRect {
    /// Validates that width/height are finite and >= 0; zero-area is
    /// allowed but later treated as `Visibility::ZeroArea` with no PTY
    /// resize.
    ///
    /// # Errors
    /// `InvalidGeometry` when non-finite or negative.
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Result<Self, RegistryError> {
        if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
            return Err(RegistryError::InvalidGeometry {
                reason: "rect components must be finite",
                rect: Self {
                    x,
                    y,
                    width,
                    height,
                },
                computed: None,
            });
        }
        if width < 0.0 || height < 0.0 {
            return Err(RegistryError::InvalidGeometry {
                reason: "rect width/height must be >= 0",
                rect: Self {
                    x,
                    y,
                    width,
                    height,
                },
                computed: None,
            });
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    #[must_use]
    pub fn is_zero_area(self) -> bool {
        self.width == 0.0 || self.height == 0.0
    }
}

// ---------------------------------------------------------------------------
// Visibility (5 states per RFC)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    Visible,
    InactiveWorkspace,
    ScratchpadHidden,
    ZeroArea,
    OverlayOccluded,
}

// ---------------------------------------------------------------------------
// Failure semantics
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum RegistryError {
    TooManyTerminals {
        max: usize,
        current: usize,
    },
    TooManyViews {
        max: usize,
        current: usize,
    },
    TooManyWorkspaces {
        max: usize,
        current: usize,
    },
    AlreadyAttached {
        terminal_id: TerminalId,
        current_view: ViewId,
    },
    ViewAlreadyAttached {
        view_id: ViewId,
        existing_terminal: TerminalId,
    },
    StaleHandle {
        expected_generation: Generation,
        found_generation: Generation,
        id_raw: u64,
    },
    RegistryDisposed {
        generation: Generation,
    },
    TerminalExited {
        terminal_id: TerminalId,
        runtime_id: RuntimeId,
        exit_code: Option<i32>,
    },
    PersistentIdInUse {
        persistent_id: PersistentId,
    },
    InvalidPersistentId {
        reason: &'static str,
        value: String,
    },
    InvalidGeometry {
        reason: &'static str,
        rect: LogicalRect,
        computed: Option<(u16, u16)>,
    },
    GenerationExhausted {
        current: Generation,
    },
    ResourceExhausted {
        reason: String,
    },
    InvalidConfig(&'static str),
    NotFound {
        kind: &'static str,
        id_raw: u64,
    },
    DetachedTerminalHasNoView {
        view_id: ViewId,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyTerminals { max, current } => {
                write!(f, "too many terminals: max {max}, current {current}")
            }
            Self::TooManyViews { max, current } => {
                write!(f, "too many views: max {max}, current {current}")
            }
            Self::TooManyWorkspaces { max, current } => {
                write!(f, "too many workspaces: max {max}, current {current}")
            }
            Self::AlreadyAttached {
                terminal_id,
                current_view,
            } => write!(
                f,
                "terminal {terminal_id} already attached to {current_view}"
            ),
            Self::ViewAlreadyAttached {
                view_id,
                existing_terminal,
            } => write!(f, "view {view_id} already hosts {existing_terminal}"),
            Self::StaleHandle {
                expected_generation,
                found_generation,
                id_raw,
            } => write!(
                f,
                "stale handle id {id_raw}: expected {expected_generation}, found {found_generation}"
            ),
            Self::RegistryDisposed { generation } => {
                write!(f, "registry disposed at {generation}")
            }
            Self::TerminalExited {
                terminal_id,
                runtime_id,
                exit_code,
            } => write!(
                f,
                "terminal {terminal_id} runtime {} exited with {:?}",
                runtime_id.0, exit_code
            ),
            Self::PersistentIdInUse { persistent_id } => {
                write!(f, "persistent id in use: {persistent_id}")
            }
            Self::InvalidPersistentId { reason, value } => {
                write!(f, "invalid persistent id {value:?}: {reason}")
            }
            Self::InvalidGeometry {
                reason,
                rect,
                computed,
            } => write!(
                f,
                "invalid geometry {rect:?} computed {computed:?}: {reason}"
            ),
            Self::GenerationExhausted { current } => {
                write!(f, "generation exhausted at {current}")
            }
            Self::ResourceExhausted { reason } => write!(f, "resource exhausted: {reason}"),
            Self::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            Self::NotFound { kind, id_raw } => write!(f, "{kind} {id_raw} not found"),
            Self::DetachedTerminalHasNoView { view_id } => {
                write!(f, "view {view_id} is not attached")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

// ---------------------------------------------------------------------------
// Config validated before registry creation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryConfig {
    pub max_terminals: usize,
    pub max_views_per_workspace: usize,
    pub max_workspaces_per_window: usize,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            max_terminals: DEFAULT_MAX_TERMINALS,
            max_views_per_workspace: DEFAULT_MAX_VIEWS_PER_WORKSPACE,
            max_workspaces_per_window: DEFAULT_MAX_WORKSPACES_PER_WINDOW,
            // CTX-0157 breathing-room cell (shared with `RuntimeConfig`).
            cell_width: crate::config::DEFAULT_CELL_WIDTH,
            cell_height: crate::config::DEFAULT_CELL_HEIGHT,
        }
    }
}

impl RegistryConfig {
    pub fn validate(&self) -> Result<(), RegistryError> {
        if !(1..=MAX_TERMINALS).contains(&self.max_terminals) {
            return Err(RegistryError::InvalidConfig(
                "max_terminals must be in [1, 64]",
            ));
        }
        if !(1..=MAX_VIEWS_PER_WORKSPACE).contains(&self.max_views_per_workspace) {
            return Err(RegistryError::InvalidConfig(
                "max_views_per_workspace must be in [1, 32]",
            ));
        }
        if !(1..=MAX_WORKSPACES_PER_WINDOW).contains(&self.max_workspaces_per_window) {
            return Err(RegistryError::InvalidConfig(
                "max_workspaces_per_window must be in [1, 16]",
            ));
        }
        if self.cell_width == 0 || self.cell_height == 0 {
            return Err(RegistryError::InvalidConfig(
                "cell_width and cell_height must be >= 1",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal records
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
struct TerminalRecord {
    id: TerminalId,
    runtime_id: RuntimeId,
    generation: Generation,
    persistent_id: Option<PersistentId>,
    state: State,
    cols: u16,
    rows: u16,
    exited: Option<Option<i32>>, // None = live, Some(exit_code)
    pending_rects: VecDeque<LogicalRect>,
    resize_coalesced: u64,
}

#[allow(dead_code)]
#[derive(Debug)]
struct Workspace {
    id: WorkspaceId,
    generation: Generation,
    layout: LayoutNode,
    focus: Focus,
    mru: VecDeque<ViewId>,
    max_views: usize,
    view_gens: HashMap<ViewId, Generation>,
    view_visibility: HashMap<ViewId, Visibility>,
    active: bool,
}

// ---------------------------------------------------------------------------
// Public handle types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerminalHandle {
    pub id: TerminalId,
    pub generation: Generation,
    pub runtime_id: RuntimeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewHandle {
    pub id: ViewId,
    pub generation: Generation,
}

// ---------------------------------------------------------------------------
// Submodules (CTX-0308 split) — facade re-exports preserve `registry::*`
// ---------------------------------------------------------------------------

mod panel;
#[cfg(test)]
mod panel_tests;
mod terminal;
#[cfg(test)]
mod tests;

pub use panel::*;
pub use terminal::*;
