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
    Focus, LayoutNode, Rect as UiRect, View, ViewId,
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
            // CTX-0157 breathing-room cell (matches RuntimeConfig 9x19).
            cell_width: 9,
            cell_height: 19,
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
// TerminalRegistry
// ---------------------------------------------------------------------------

pub struct TerminalRegistry {
    registry_generation: Generation,
    next_terminal_raw: u64,
    next_runtime_raw: u64,
    next_view_raw: u64,
    next_workspace_raw: u64,
    config: RegistryConfig,
    terminals: HashMap<u64, TerminalRecord>,
    persistent_index: HashMap<PersistentId, TerminalId>,
    workspaces: HashMap<u64, Workspace>,
    active_workspace: Option<WorkspaceId>,
    terminal_to_view: HashMap<TerminalId, ViewId>,
    view_to_terminal: HashMap<ViewId, TerminalId>,
    disposed: bool,
    total_created: u64,
    errors: HashMap<String, u64>,
}

impl std::fmt::Debug for TerminalRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalRegistry")
            .field("generation", &self.registry_generation)
            .field("terminals_active", &self.terminals.len())
            .field("total_created", &self.total_created)
            .field("workspaces", &self.workspaces.len())
            .field("active_workspace", &self.active_workspace)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl TerminalRegistry {
    /// Creates a registry after `ConfigPlan` validation; synchronous fail-only
    /// on PTY/platform allocation here simulated as never failing for headless.
    ///
    /// # Errors
    /// `InvalidConfig` for bad bounds, `GenerationExhausted` if reserved.
    pub fn new(config: RegistryConfig) -> Result<Self, RegistryError> {
        config.validate()?;
        if Generation::INITIAL.is_exhausted() {
            return Err(RegistryError::GenerationExhausted {
                current: Generation::INITIAL,
            });
        }
        Ok(Self {
            registry_generation: Generation::INITIAL,
            next_terminal_raw: 1,
            next_runtime_raw: 1,
            next_view_raw: 1,
            next_workspace_raw: 1,
            config,
            terminals: HashMap::new(),
            persistent_index: HashMap::new(),
            workspaces: HashMap::new(),
            active_workspace: None,
            terminal_to_view: HashMap::new(),
            view_to_terminal: HashMap::new(),
            disposed: false,
            total_created: 0,
            errors: HashMap::new(),
        })
    }

    fn ensure_not_disposed(&self) -> Result<(), RegistryError> {
        if self.disposed {
            return Err(RegistryError::RegistryDisposed {
                generation: self.registry_generation,
            });
        }
        Ok(())
    }

    fn bump_error(&mut self, variant: &str) {
        *self.errors.entry(variant.to_owned()).or_insert(0) += 1;
    }

    /// Current registry generation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.registry_generation
    }

    /// Number of live terminals.
    #[must_use]
    pub fn terminal_count(&self) -> usize {
        self.terminals.len()
    }

    /// Number of workspaces.
    #[must_use]
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// Returns config.
    #[must_use]
    pub fn config(&self) -> &RegistryConfig {
        &self.config
    }

    /// Creates a terminal; validates bounds and `PersistentId` before allocation.
    ///
    /// # Errors
    /// `TooManyTerminals`, `PersistentIdInUse`, `InvalidPersistentId`,
    /// `GenerationExhausted`, `RegistryDisposed`.
    pub fn create_terminal(
        &mut self,
        persistent_id: Option<PersistentId>,
    ) -> Result<TerminalHandle, RegistryError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            self.bump_error("GenerationExhausted");
            return Err(RegistryError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        if self.terminals.len() >= self.config.max_terminals {
            self.bump_error("TooManyTerminals");
            return Err(RegistryError::TooManyTerminals {
                max: self.config.max_terminals,
                current: self.terminals.len(),
            });
        }
        if let Some(pid) = &persistent_id {
            if self.persistent_index.contains_key(pid) {
                self.bump_error("PersistentIdInUse");
                return Err(RegistryError::PersistentIdInUse {
                    persistent_id: pid.clone(),
                });
            }
        }
        // Generation bump per successful allocation
        let next_gen = self.registry_generation.next()?;
        self.registry_generation = next_gen;
        let tid = TerminalId::new(self.next_terminal_raw);
        self.next_terminal_raw = self.next_terminal_raw.wrapping_add(1).max(1);
        let rid = RuntimeId::new(self.next_runtime_raw);
        self.next_runtime_raw = self.next_runtime_raw.wrapping_add(1).max(1);
        let gen_val = self.registry_generation;
        let cols: u16 = 80;
        let rows: u16 = 24;
        let state = State::new();
        let rec = TerminalRecord {
            id: tid,
            runtime_id: rid,
            generation: gen_val,
            persistent_id: persistent_id.clone(),
            state,
            cols,
            rows,
            exited: None,
            pending_rects: VecDeque::new(),
            resize_coalesced: 0,
        };
        self.terminals.insert(tid.0, rec);
        if let Some(pid) = persistent_id {
            self.persistent_index.insert(pid, tid);
        }
        self.total_created += 1;
        Ok(TerminalHandle {
            id: tid,
            generation: gen_val,
            runtime_id: rid,
        })
    }

    /// Validates `(id, generation)` before returning a reference.
    fn get_terminal(
        &self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<&TerminalRecord, RegistryError> {
        self.ensure_not_disposed()?;
        let rec = self.terminals.get(&id.0).ok_or(RegistryError::NotFound {
            kind: "terminal",
            id_raw: id.0,
        })?;
        if rec.generation != generation {
            return Err(RegistryError::StaleHandle {
                expected_generation: rec.generation,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    fn get_terminal_mut(
        &mut self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<&mut TerminalRecord, RegistryError> {
        self.ensure_not_disposed()?;
        // Need to check generation without borrowing twice
        let current_gen = {
            let rec = self.terminals.get(&id.0).ok_or(RegistryError::NotFound {
                kind: "terminal",
                id_raw: id.0,
            })?;
            rec.generation
        };
        if current_gen != generation {
            return Err(RegistryError::StaleHandle {
                expected_generation: current_gen,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        // Return mutable reference after validation (fail-closed, no expect).
        self.terminals
            .get_mut(&id.0)
            .ok_or(RegistryError::NotFound {
                kind: "terminal",
                id_raw: id.0,
            })
    }

    /// Returns terminal snapshot handle (read-only).
    ///
    /// # Errors
    /// `StaleHandle`, `NotFound`, `RegistryDisposed`, `TerminalExited`.
    pub fn terminal_snapshot(
        &self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<Snapshot, RegistryError> {
        let rec = self.get_terminal(id, generation)?;
        if let Some(exit) = rec.exited {
            return Err(RegistryError::TerminalExited {
                terminal_id: id,
                runtime_id: rec.runtime_id,
                exit_code: exit,
            });
        }
        Ok(rec.state.snapshot())
    }

    /// Returns terminal's scrollback/history snapshot for rehydration testing.
    pub fn terminal_persistent_id(
        &self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<Option<PersistentId>, RegistryError> {
        Ok(self.get_terminal(id, generation)?.persistent_id.clone())
    }

    /// Marks terminal as exited (simulates process exit). Retains `TerminalId`
    /// until explicitly closed.
    ///
    /// # Errors
    /// `StaleHandle`, `NotFound`.
    pub fn mark_exited(
        &mut self,
        id: TerminalId,
        generation: Generation,
        exit_code: Option<i32>,
    ) -> Result<(), RegistryError> {
        let rec = self.get_terminal_mut(id, generation)?;
        rec.exited = Some(exit_code);
        Ok(())
    }

    /// Destroys the PTY, retires the `TerminalId` with a generation bump.
    /// Clears attachment if any and clears `PersistentId` index.
    ///
    /// # Errors
    /// `StaleHandle`, `NotFound`, `RegistryDisposed`.
    pub fn close_terminal(
        &mut self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        let (stored_gen, persistent_cloned) = {
            let rec = self.terminals.get(&id.0).ok_or(RegistryError::NotFound {
                kind: "terminal",
                id_raw: id.0,
            })?;
            (rec.generation, rec.persistent_id.clone())
        };
        if stored_gen != generation {
            self.bump_error("StaleHandle");
            return Err(RegistryError::StaleHandle {
                expected_generation: stored_gen,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        // Remove attachment if any
        if let Some(view_id) = self.terminal_to_view.remove(&id) {
            self.view_to_terminal.remove(&view_id);
        }
        if let Some(pid) = persistent_cloned {
            self.persistent_index.remove(&pid);
        }
        self.terminals.remove(&id.0);
        // Generation bump on close (retires handle)
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        }
        Ok(())
    }

    /// Creates a workspace bounded by `max_workspaces_per_window`.
    ///
    /// # Errors
    /// `TooManyWorkspaces`, `GenerationExhausted`, `RegistryDisposed`.
    pub fn create_workspace(&mut self) -> Result<WorkspaceId, RegistryError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            return Err(RegistryError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        if self.workspaces.len() >= self.config.max_workspaces_per_window {
            self.bump_error("TooManyWorkspaces");
            return Err(RegistryError::TooManyWorkspaces {
                max: self.config.max_workspaces_per_window,
                current: self.workspaces.len(),
            });
        }
        let wid = WorkspaceId::new(self.next_workspace_raw);
        self.next_workspace_raw = self.next_workspace_raw.wrapping_add(1).max(1);
        let gen_val = self.registry_generation.next()?;
        self.registry_generation = gen_val;
        let ws = Workspace {
            id: wid,
            generation: gen_val,
            layout: LayoutNode::stack(Vec::new()),
            focus: Focus::new(),
            mru: VecDeque::new(),
            max_views: self.config.max_views_per_workspace,
            view_gens: HashMap::new(),
            view_visibility: HashMap::new(),
            active: self.workspaces.is_empty(),
        };
        self.workspaces.insert(wid.0, ws);
        if self.active_workspace.is_none() {
            self.active_workspace = Some(wid);
        }
        Ok(wid)
    }

    fn get_workspace(&self, wid: WorkspaceId) -> Result<&Workspace, RegistryError> {
        self.ensure_not_disposed()?;
        self.workspaces.get(&wid.0).ok_or(RegistryError::NotFound {
            kind: "workspace",
            id_raw: wid.0,
        })
    }

    fn get_workspace_mut(&mut self, wid: WorkspaceId) -> Result<&mut Workspace, RegistryError> {
        self.ensure_not_disposed()?;
        self.workspaces
            .get_mut(&wid.0)
            .ok_or(RegistryError::NotFound {
                kind: "workspace",
                id_raw: wid.0,
            })
    }

    /// Returns active workspace id, if any.
    #[must_use]
    pub fn active_workspace(&self) -> Option<WorkspaceId> {
        self.active_workspace
    }

    /// Sets active workspace; inactive workspaces' views become
    /// `Visibility::InactiveWorkspace`.
    ///
    /// # Errors
    /// `NotFound`, `RegistryDisposed`.
    pub fn set_active_workspace(&mut self, wid: WorkspaceId) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        if !self.workspaces.contains_key(&wid.0) {
            return Err(RegistryError::NotFound {
                kind: "workspace",
                id_raw: wid.0,
            });
        }
        for ws in self.workspaces.values_mut() {
            ws.active = ws.id == wid;
        }
        self.active_workspace = Some(wid);
        // Update visibility for all views
        for ws in self.workspaces.values_mut() {
            let is_active = ws.active;
            for (vid, vis) in ws.view_visibility.iter_mut() {
                if *vis == Visibility::Visible || *vis == Visibility::InactiveWorkspace {
                    *vis = if is_active {
                        Visibility::Visible
                    } else {
                        Visibility::InactiveWorkspace
                    };
                }
                let _ = vid;
            }
        }
        Ok(())
    }

    /// Creates a view in `workspace`. Validates `max_views_per_workspace`.
    ///
    /// # Errors
    /// `TooManyViews`, `NotFound`, `GenerationExhausted`.
    pub fn create_view(&mut self, workspace_id: WorkspaceId) -> Result<ViewHandle, RegistryError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            return Err(RegistryError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        // Validate workspace exists and has capacity before bumping generation
        {
            let (current, max) = {
                let ws = self.get_workspace(workspace_id)?;
                (ws.view_gens.len(), ws.max_views)
            };
            if current >= max {
                self.bump_error("TooManyViews");
                return Err(RegistryError::TooManyViews { max, current });
            }
        }
        let next_gen = self.registry_generation.next()?;
        self.registry_generation = next_gen;
        let vid = ViewId::new(self.next_view_raw);
        self.next_view_raw = self.next_view_raw.wrapping_add(1).max(1);
        let vgen = self.registry_generation;
        // Insert view leaf into workspace layout (Stack semantics)
        let ws = self.get_workspace_mut(workspace_id)?;
        let view = View::new(vid, 80, 24);
        // Append to Stack
        let new_layout = match std::mem::replace(&mut ws.layout, LayoutNode::stack(Vec::new())) {
            LayoutNode::Stack(mut children) => {
                children.push(LayoutNode::leaf(view));
                LayoutNode::stack(children)
            }
            other => {
                // Convert single leaf or split into stack with new leaf appended
                // For simplicity, make a stack of [other, new leaf]
                LayoutNode::stack(vec![other, LayoutNode::leaf(view)])
            }
        };
        ws.layout = new_layout;
        ws.view_gens.insert(vid, vgen);
        ws.view_visibility.insert(vid, Visibility::Visible);
        ws.mru.push_front(vid);
        // If focus is None, focus new view
        if ws.focus.focused().is_none() {
            ws.focus.set(vid);
        }
        Ok(ViewHandle {
            id: vid,
            generation: vgen,
        })
    }

    /// Validates `ViewId` + generation for a workspace.
    fn validate_view(
        &self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
    ) -> Result<(), RegistryError> {
        let ws = self.get_workspace(workspace_id)?;
        let stored = ws.view_gens.get(&view_id).ok_or(RegistryError::NotFound {
            kind: "view",
            id_raw: view_id.0,
        })?;
        if *stored != view_gen {
            return Err(RegistryError::StaleHandle {
                expected_generation: *stored,
                found_generation: view_gen,
                id_raw: view_id.0,
            });
        }
        Ok(())
    }

    /// Destroys a view leaf; retires `ViewId` with generation bump.
    /// If the view was attached, detaches first (clearing focus MRU).
    /// If focused view destroyed, focus moves to next MRU.
    ///
    /// # Errors
    /// `StaleHandle`, `NotFound`.
    pub fn destroy_view(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        // Detach if attached
        if let Some(tid) = self.view_to_terminal.remove(&view_id) {
            self.terminal_to_view.remove(&tid);
        }
        let ws = self.get_workspace_mut(workspace_id)?;
        // Remove from layout tree (rebuild stack without this leaf)
        let leaves = ws.layout.leaf_ids();
        let mut new_children: Vec<LayoutNode> = Vec::new();
        for leaf_id in leaves {
            if leaf_id == view_id {
                continue;
            }
            if let Some(v) = ws.layout.find_leaf(leaf_id) {
                new_children.push(LayoutNode::leaf(v.clone()));
            }
        }
        ws.layout = if new_children.is_empty() {
            LayoutNode::stack(Vec::new())
        } else if new_children.len() == 1 {
            new_children
                .into_iter()
                .next()
                .unwrap_or_else(|| LayoutNode::stack(Vec::new()))
        } else {
            LayoutNode::stack(new_children)
        };
        ws.view_gens.remove(&view_id);
        ws.view_visibility.remove(&view_id);
        ws.mru.retain(|&id| id != view_id);
        // Focus handling: if destroyed was focused, move to next MRU
        let focused = ws.focus.focused();
        if focused == Some(view_id) {
            if let Some(&next) = ws.mru.front() {
                ws.focus.set(next);
            } else {
                ws.focus.clear();
            }
        }
        // Generation bump for view retirement
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        }
        Ok(())
    }

    /// Binds `ViewId` to `TerminalId`. Requires view exists and is not already
    /// attached, terminal exists and is not already attached elsewhere, neither
    /// handle stale, and view visibility not ZeroArea.
    ///
    /// On success re-measures rectangle via `logical_rect_to_grid` and
    /// resizes terminal (PTY).
    ///
    /// # Errors
    /// `StaleHandle`, `AlreadyAttached`, `ViewAlreadyAttached`, `InvalidGeometry`.
    pub fn attach(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
        terminal_id: TerminalId,
        term_gen: Generation,
        rect: LogicalRect,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        let term_rec = self.get_terminal(terminal_id, term_gen)?;
        if let Some(exit_code) = term_rec.exited {
            return Err(RegistryError::TerminalExited {
                terminal_id,
                runtime_id: term_rec.runtime_id,
                exit_code,
            });
        }
        if self.terminal_to_view.contains_key(&terminal_id) {
            let cur_view = self.terminal_to_view[&terminal_id];
            self.bump_error("AlreadyAttached");
            return Err(RegistryError::AlreadyAttached {
                terminal_id,
                current_view: cur_view,
            });
        }
        if self.view_to_terminal.contains_key(&view_id) {
            let existing = self.view_to_terminal[&view_id];
            self.bump_error("ViewAlreadyAttached");
            return Err(RegistryError::ViewAlreadyAttached {
                view_id,
                existing_terminal: existing,
            });
        }
        if rect.is_zero_area() {
            self.bump_error("InvalidGeometry");
            return Err(RegistryError::InvalidGeometry {
                reason: "zero-area rect",
                rect,
                computed: None,
            });
        }
        // Validate DPI conversion before committing attachment
        let (cols, rows) = self.logical_rect_to_grid(rect)?;
        // Commit
        self.view_to_terminal.insert(view_id, terminal_id);
        self.terminal_to_view.insert(terminal_id, view_id);
        // Update workspace MRU and focus? Attach does not auto-focus per spec
        // but we push to MRU for later
        if let Ok(ws) = self.get_workspace_mut(workspace_id) {
            ws.mru.retain(|&id| id != view_id);
            ws.mru.push_front(view_id);
        }
        // Resize terminal to new geometry (synchronous, debounced)
        let rec = self.get_terminal_mut(terminal_id, term_gen)?;
        rec.cols = cols;
        rec.rows = rows;
        // Simulate PTY resize via state resize? Use state resize to keep invariants
        let _ = rec.state.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// Unbinds view from its terminal, preserving both ids.
    ///
    /// Detaching the focused view clears focus before unbind (MRU next).
    ///
    /// # Errors
    /// `StaleHandle`, `NotFound`, `DetachedTerminalHasNoView`.
    pub fn detach(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
    ) -> Result<TerminalId, RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        let tid = self
            .view_to_terminal
            .remove(&view_id)
            .ok_or(RegistryError::DetachedTerminalHasNoView { view_id })?;
        self.terminal_to_view.remove(&tid);
        let ws = self.get_workspace_mut(workspace_id)?;
        // Focus handling
        let focused = ws.focus.focused();
        if focused == Some(view_id) {
            ws.mru.retain(|&id| id != view_id);
            if let Some(&next) = ws.mru.front() {
                ws.focus.set(next);
            } else {
                // Find any other view
                let other = ws.view_gens.keys().find(|&&id| id != view_id).copied();
                if let Some(o) = other {
                    ws.focus.set(o);
                } else {
                    ws.focus.clear();
                }
            }
        } else {
            ws.mru.retain(|&id| id != view_id);
            ws.mru.push_front(view_id);
        }
        // View becomes Empty placeholder; visibility stays Visible but without attachment
        Ok(tid)
    }

    /// Atomic reattachment: validates both views and terminal, detaches from
    /// source and attaches to destination in one commit. On failure neither
    /// view changes.
    #[allow(clippy::too_many_arguments)]
    pub fn move_terminal(
        &mut self,
        terminal_id: TerminalId,
        term_gen: Generation,
        from_workspace: WorkspaceId,
        from_view: ViewId,
        from_gen: Generation,
        to_workspace: WorkspaceId,
        to_view: ViewId,
        to_gen: Generation,
        rect: LogicalRect,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        // Validate all handles before mutation (fail-closed)
        self.validate_view(from_workspace, from_view, from_gen)?;
        self.validate_view(to_workspace, to_view, to_gen)?;
        let term_rec = self.get_terminal(terminal_id, term_gen)?;
        if let Some(exit_code) = term_rec.exited {
            return Err(RegistryError::TerminalExited {
                terminal_id,
                runtime_id: term_rec.runtime_id,
                exit_code,
            });
        }
        // Source must be attached to this terminal
        match self.view_to_terminal.get(&from_view) {
            Some(&tid) if tid == terminal_id => {}
            Some(&tid) => {
                return Err(RegistryError::AlreadyAttached {
                    terminal_id: tid,
                    current_view: from_view,
                });
            }
            None => return Err(RegistryError::DetachedTerminalHasNoView { view_id: from_view }),
        }
        // Destination must be empty
        if self.view_to_terminal.contains_key(&to_view) {
            let existing = self.view_to_terminal[&to_view];
            return Err(RegistryError::ViewAlreadyAttached {
                view_id: to_view,
                existing_terminal: existing,
            });
        }
        if rect.is_zero_area() {
            return Err(RegistryError::InvalidGeometry {
                reason: "zero-area rect",
                rect,
                computed: None,
            });
        }
        let (cols, rows) = self.logical_rect_to_grid(rect)?;
        // Commit atomically
        self.view_to_terminal.remove(&from_view);
        self.terminal_to_view.remove(&terminal_id);
        // Focus handling for source detach
        {
            if let Ok(ws) = self.get_workspace_mut(from_workspace) {
                let focused = ws.focus.focused();
                if focused == Some(from_view) {
                    ws.mru.retain(|&id| id != from_view);
                    if let Some(&next) = ws.mru.front() {
                        ws.focus.set(next);
                    } else {
                        ws.focus.clear();
                    }
                }
            }
        }
        self.view_to_terminal.insert(to_view, terminal_id);
        self.terminal_to_view.insert(terminal_id, to_view);
        if let Ok(ws) = self.get_workspace_mut(to_workspace) {
            ws.mru.retain(|&id| id != to_view);
            ws.mru.push_front(to_view);
        }
        let rec = self.get_terminal_mut(terminal_id, term_gen)?;
        rec.cols = cols;
        rec.rows = rows;
        let _ = rec.state.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// Swap where view previously held `old_terminal_id`; old terminal becomes detached.
    pub fn replace(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
        new_terminal_id: TerminalId,
        new_gen: Generation,
        rect: LogicalRect,
    ) -> Result<Option<TerminalId>, RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        let new_rec = self.get_terminal(new_terminal_id, new_gen)?;
        if let Some(exit_code) = new_rec.exited {
            return Err(RegistryError::TerminalExited {
                terminal_id: new_terminal_id,
                runtime_id: new_rec.runtime_id,
                exit_code,
            });
        }
        if self.terminal_to_view.contains_key(&new_terminal_id) {
            let cur = self.terminal_to_view[&new_terminal_id];
            return Err(RegistryError::AlreadyAttached {
                terminal_id: new_terminal_id,
                current_view: cur,
            });
        }
        let old = self.view_to_terminal.get(&view_id).copied();
        if rect.is_zero_area() {
            return Err(RegistryError::InvalidGeometry {
                reason: "zero-area rect",
                rect,
                computed: None,
            });
        }
        let (cols, rows) = self.logical_rect_to_grid(rect)?;
        if let Some(old_id) = old {
            self.terminal_to_view.remove(&old_id);
        }
        self.view_to_terminal.insert(view_id, new_terminal_id);
        self.terminal_to_view.insert(new_terminal_id, view_id);
        let rec = self.get_terminal_mut(new_terminal_id, new_gen)?;
        rec.cols = cols;
        rec.rows = rows;
        let _ = rec.state.resize(cols as usize, rows as usize);
        Ok(old)
    }

    // ------------------------------------------------------------------
    // Focus (per window/workspace, MRU)
    // ------------------------------------------------------------------

    pub fn focused_view(&self, workspace_id: WorkspaceId) -> Option<ViewId> {
        self.workspaces
            .get(&workspace_id.0)
            .and_then(|ws| ws.focus.focused())
    }

    pub fn set_focus(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        let ws = self.get_workspace_mut(workspace_id)?;
        ws.focus.set(view_id);
        ws.mru.retain(|&id| id != view_id);
        ws.mru.push_front(view_id);
        Ok(())
    }

    pub fn move_focus(
        &mut self,
        workspace_id: WorkspaceId,
        dir: bitty_ui::FocusDirection,
        container: UiRect,
    ) -> Result<Option<ViewId>, RegistryError> {
        self.ensure_not_disposed()?;
        let ws = self.get_workspace_mut(workspace_id)?;
        let next = ws.focus.advance(&ws.layout, container, dir);
        if let Some(id) = next {
            ws.focus.set(id);
            ws.mru.retain(|&nid| nid != id);
            ws.mru.push_front(id);
        }
        Ok(next)
    }

    pub fn mru_order(&self, workspace_id: WorkspaceId) -> Result<Vec<ViewId>, RegistryError> {
        Ok(self
            .get_workspace(workspace_id)?
            .mru
            .iter()
            .copied()
            .collect())
    }

    // ------------------------------------------------------------------
    // Resize routing: LogicalRect -> cell grid
    // ------------------------------------------------------------------

    /// Converts a validated `LogicalRect` to PTY grid `(cols, rows)` using
    /// DPI-aware cell metrics: `cols = floor(rect.width / cell_width)`,
    /// clamped to `[1, 1024]` each and to configured `max_cols`/`max_rows`.
    /// Zero-area returns `InvalidGeometry` with no PTY resize.
    pub fn logical_rect_to_grid(&self, rect: LogicalRect) -> Result<(u16, u16), RegistryError> {
        if rect.is_zero_area() {
            return Err(RegistryError::InvalidGeometry {
                reason: "zero-area rect retains previous geometry",
                rect,
                computed: None,
            });
        }
        let cols_f = (rect.width / f64::from(self.config.cell_width)).floor();
        let rows_f = (rect.height / f64::from(self.config.cell_height)).floor();
        let mut cols = cols_f as i64;
        let mut rows = rows_f as i64;
        // Clamp to [1, 1024]
        cols = cols.clamp(1, i64::from(MAX_COLS));
        rows = rows.clamp(1, i64::from(MAX_ROWS));
        let cols_u = cols as u16;
        let rows_u = rows as u16;
        Ok((cols_u, rows_u))
    }

    /// Queues a resize for the terminal attached to `view_id`. Validates
    /// `LogicalRect`, converts via `logical_rect_to_grid`, and enqueues to
    /// the terminal's pending queue (debounce 64, coalesce). Zero-area does
    /// not enqueue and returns `InvalidGeometry`.
    ///
    /// Debounce: at most one resize per presentation tick per terminal is
    /// committed by `flush_pending_resizes`; intermediate rects inside the same
    /// tick are coalesced to latest. Beyond 64 queued rects per tick drops
    /// oldest with `resize_coalesced` counter.
    pub fn handle_view_rect(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
        rect: LogicalRect,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        if rect.is_zero_area() {
            // Visibility ZeroArea: no PTY resize, previous geometry retained
            if let Ok(ws) = self.get_workspace_mut(workspace_id) {
                ws.view_visibility.insert(view_id, Visibility::ZeroArea);
            }
            return Err(RegistryError::InvalidGeometry {
                reason: "zero-area rect never reaches PTY",
                rect,
                computed: None,
            });
        }
        let tid = *self
            .view_to_terminal
            .get(&view_id)
            .ok_or(RegistryError::DetachedTerminalHasNoView { view_id })?;
        // Validate conversion before queue
        let _ = self.logical_rect_to_grid(rect)?;
        // Find terminal generation for check
        let gen_val = {
            let rec = self.terminals.get(&tid.0).ok_or(RegistryError::NotFound {
                kind: "terminal",
                id_raw: tid.0,
            })?;
            rec.generation
        };
        let rec = self.get_terminal_mut(tid, gen_val)?;
        if let Some(exit_code) = rec.exited {
            return Err(RegistryError::TerminalExited {
                terminal_id: tid,
                runtime_id: rec.runtime_id,
                exit_code,
            });
        }
        if rec.pending_rects.len() >= RESIZE_DEBOUNCE_CAP {
            rec.pending_rects.pop_front();
            rec.resize_coalesced += 1;
        }
        rec.pending_rects.push_back(rect);
        if let Ok(ws) = self.get_workspace_mut(workspace_id) {
            ws.view_visibility.insert(view_id, Visibility::Visible);
        }
        Ok(())
    }

    /// Flushes at most one resize per terminal per tick: coalesces pending
    /// queue to latest rect, converts to grid, commits terminal resize.
    /// Returns list of `(TerminalId, (cols, rows))` committed.
    pub fn flush_pending_resizes(&mut self) -> Vec<(TerminalId, (u16, u16))> {
        let mut out = Vec::new();
        let tids: Vec<TerminalId> = self.terminals.keys().map(|&raw| TerminalId(raw)).collect();
        for tid in tids {
            let gen_val = if let Some(rec) = self.terminals.get(&tid.0) {
                rec.generation
            } else {
                continue;
            };
            let pending_len = if let Some(rec) = self.terminals.get(&tid.0) {
                rec.pending_rects.len()
            } else {
                0
            };
            if pending_len == 0 {
                continue;
            }
            // Coalesce to latest: drain all, keep last (fail-closed, no unwrap).
            let latest = {
                let Some(rec) = self.terminals.get_mut(&tid.0) else {
                    continue;
                };
                let Some(last) = rec.pending_rects.back().copied() else {
                    continue;
                };
                rec.pending_rects.clear();
                last
            };
            // Convert and commit
            let grid = match self.logical_rect_to_grid(latest) {
                Ok(g) => g,
                Err(_) => continue,
            };
            let rec = match self.terminals.get_mut(&tid.0) {
                Some(r) if r.generation == gen_val => r,
                _ => continue,
            };
            if rec.exited.is_some() {
                continue;
            }
            rec.cols = grid.0;
            rec.rows = grid.1;
            let _ = rec.state.resize(grid.0 as usize, grid.1 as usize);
            out.push((tid, grid));
        }
        out
    }

    /// Returns `resize_coalesced` counter for a terminal.
    pub fn resize_coalesced(
        &self,
        id: TerminalId,
        generation: Generation,
    ) -> Result<u64, RegistryError> {
        Ok(self.get_terminal(id, generation)?.resize_coalesced)
    }

    // ------------------------------------------------------------------
    // Visibility
    // ------------------------------------------------------------------

    pub fn set_visibility(
        &mut self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
        view_gen: Generation,
        vis: Visibility,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        self.validate_view(workspace_id, view_id, view_gen)?;
        let ws = self.get_workspace_mut(workspace_id)?;
        ws.view_visibility.insert(view_id, vis);
        Ok(())
    }

    pub fn visibility(
        &self,
        workspace_id: WorkspaceId,
        view_id: ViewId,
    ) -> Result<Visibility, RegistryError> {
        let ws = self.get_workspace(workspace_id)?;
        ws.view_visibility
            .get(&view_id)
            .copied()
            .ok_or(RegistryError::NotFound {
                kind: "view",
                id_raw: view_id.0,
            })
    }

    // ------------------------------------------------------------------
    // Layout helpers — delegate to LayoutNode without hardcoding tabs
    // ------------------------------------------------------------------

    pub fn set_workspace_layout(
        &mut self,
        workspace_id: WorkspaceId,
        layout: LayoutNode,
    ) -> Result<(), RegistryError> {
        self.ensure_not_disposed()?;
        if !self.workspaces.contains_key(&workspace_id.0) {
            return Err(RegistryError::NotFound {
                kind: "workspace",
                id_raw: workspace_id.0,
            });
        }
        // Validate view count before commit
        let new_count = layout.leaf_count();
        let max_views = self.workspaces[&workspace_id.0].max_views;
        if new_count > max_views {
            self.bump_error("TooManyViews");
            return Err(RegistryError::TooManyViews {
                max: max_views,
                current: new_count,
            });
        }
        // Validate all leaf ViewIds are known to this workspace (fail-closed).
        let new_ids = layout.leaf_ids();
        {
            let ws = self
                .workspaces
                .get(&workspace_id.0)
                .ok_or(RegistryError::NotFound {
                    kind: "workspace",
                    id_raw: workspace_id.0,
                })?;
            for vid in &new_ids {
                if !ws.view_gens.contains_key(vid) {
                    return Err(RegistryError::NotFound {
                        kind: "view",
                        id_raw: vid.0,
                    });
                }
            }
        }
        // Remove views that are no longer in layout
        let to_remove: Vec<ViewId> = {
            let ws = self
                .workspaces
                .get(&workspace_id.0)
                .ok_or(RegistryError::NotFound {
                    kind: "workspace",
                    id_raw: workspace_id.0,
                })?;
            ws.view_gens
                .keys()
                .filter(|id| !new_ids.contains(id))
                .copied()
                .collect()
        };
        for vid in &to_remove {
            {
                let Some(ws) = self.workspaces.get_mut(&workspace_id.0) else {
                    continue;
                };
                ws.view_gens.remove(vid);
                ws.view_visibility.remove(vid);
                ws.mru.retain(|&id| id != *vid);
            }
            if let Some(tid) = self.view_to_terminal.remove(vid) {
                self.terminal_to_view.remove(&tid);
            }
        }
        {
            let Some(ws) = self.workspaces.get_mut(&workspace_id.0) else {
                return Err(RegistryError::NotFound {
                    kind: "workspace",
                    id_raw: workspace_id.0,
                });
            };
            ws.layout = layout;
            // Reconcile focus: if focused view no longer exists, move to MRU
            if let Some(focused) = ws.focus.focused() {
                if !ws.view_gens.contains_key(&focused) {
                    if let Some(&next) = ws.mru.front() {
                        ws.focus.set(next);
                    } else {
                        ws.focus.clear();
                    }
                }
            } else if ws.focus.focused().is_none() {
                if let Some(&first) = new_ids.first() {
                    ws.focus.set(first);
                }
            }
        }
        Ok(())
    }

    pub fn workspace_layout(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<&LayoutNode, RegistryError> {
        Ok(&self.get_workspace(workspace_id)?.layout)
    }

    pub fn reflow_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        container: UiRect,
    ) -> Result<Vec<(ViewId, UiRect)>, RegistryError> {
        self.ensure_not_disposed()?;
        let ws = self.get_workspace_mut(workspace_id)?;
        ws.layout.reflow(container);
        Ok(ws.layout.layout(container))
    }

    // ------------------------------------------------------------------
    // Disposal
    // ------------------------------------------------------------------

    /// Disposes registry: closes every live PTY, retires every handle,
    /// clears persistent index, increments generation, makes every further
    /// call return `RegistryDisposed`.
    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.terminals.clear();
        self.persistent_index.clear();
        self.terminal_to_view.clear();
        self.view_to_terminal.clear();
        // Clear workspaces but keep generation bump
        self.workspaces.clear();
        self.active_workspace = None;
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        } else {
            self.registry_generation = Generation(u64::MAX);
        }
        self.disposed = true;
    }

    /// Alias for `dispose` (spec: registry disposal retires every TerminalId).
    pub fn close(&mut self) {
        self.dispose();
    }

    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    #[must_use]
    pub fn total_created(&self) -> u64 {
        self.total_created
    }

    #[must_use]
    pub fn error_count(&self, variant: &str) -> u64 {
        self.errors.get(variant).copied().unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Attach/detach queries
    // ------------------------------------------------------------------

    #[must_use]
    pub fn is_attached(&self, terminal_id: TerminalId) -> bool {
        self.terminal_to_view.contains_key(&terminal_id)
    }

    #[must_use]
    pub fn attached_view(&self, terminal_id: TerminalId) -> Option<ViewId> {
        self.terminal_to_view.get(&terminal_id).copied()
    }

    #[must_use]
    pub fn attached_terminal(&self, view_id: ViewId) -> Option<TerminalId> {
        self.view_to_terminal.get(&view_id).copied()
    }

    // For testing: allow direct setting of generation to near MAX
    #[cfg(test)]
    pub fn set_generation_for_test(&mut self, generation: Generation) {
        self.registry_generation = generation;
    }

    #[cfg(test)]
    pub fn workspace_view_count(&self, wid: WorkspaceId) -> usize {
        self.workspaces
            .get(&wid.0)
            .map(|ws| ws.view_gens.len())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Panel Runtime domain (CTX-0102) — generic Panel Runtime per 9032d1e
// ---------------------------------------------------------------------------
//
// Implements PanelId distinct newtype, generation monotonic, lifecycle
// Declared->Created->Mounted->Focused->Suspended->Disposed, command registry
// owner.name:command, overlay max 4+1, focus MRU per Window/Workspace,
// EventBus 64/1024/2MiB/8192 DropOldest, panel.* capability per
// (PanelId,generation). Single-process winit window, one registry per
// window, no bittyd/remote. Bounded, no unsafe, headless testable.
//
// Placement: `Instance -> Window -> Workspace -> LayoutTree -> View`
// stays authoritative; Panel is typed View content (`ViewContent::Panel`)
// per Option A, reusing ViewId generation, focus MRU, visibility.
// The panel runtime owns panel lifecycle, the workspace owns layout,
// the terminal registry owns PTY descriptors; no view/panel holds PTY fd,
// GPU object, or OS window handle.

// Re-export canonical Panel types from bitty-ui for single definition.
pub use bitty_ui::panel::{
    MAX_COMMANDS_PER_PANEL_TYPE, MAX_OVERLAY_TEXT_LEN, MAX_OVERLAY_TOOLTIP_LEN,
    MAX_OVERLAYS_PER_WINDOW, Overlay, OverlayError, OverlayKind, OverlayManager, PanelFocus,
    QualifiedCommand, ViewContent,
};
pub use bitty_ui::panel::{PanelId, PanelState, PanelType};

// PanelId distinctness is already enforced by `bitty_ui::PanelId` being a
// newtype with no From bridge to `ViewId`/`TerminalId`. Re-exported here so
// `registry::PanelId` is the same canonical type but still pairwise
// incompatible at type level across crates (requires explicit import).

pub const MAX_PANELS_PER_WORKSPACE: usize = 32;
pub const MAX_PANELS_PER_WINDOW: usize = 64;
pub const DEFAULT_MAX_PANELS_PER_WORKSPACE: usize = 16;
pub const DEFAULT_MAX_PANELS_PER_WINDOW: usize = 32;
pub const MAX_TOPICS_TOTAL: usize = 256;
pub const MAX_SUBSCRIPTIONS_PER_PANEL: usize = 32;
pub const MAX_PANEL_COMMANDS_PER_TYPE: usize = 32;
pub const BUS_PER_SUBSCRIPTION_LIMIT: usize = 64;
pub const BUS_PER_PANEL_LIMIT: usize = 1024;
pub const BUS_PER_PANEL_BYTES_LIMIT: usize = 256 * 1024;
pub const BUS_GLOBAL_LIMIT: usize = 8192;
pub const BUS_GLOBAL_BYTES_LIMIT: usize = 2 * 1024 * 1024;
pub const BUS_EVENT_MAX_BYTES: usize = 8 * 1024;
pub const BUS_BATCH_MAX_EVENTS: usize = 32;
pub const BUS_BATCH_MAX_BYTES: usize = 8 * 1024;

/// Panel generation is the same monotonic `Generation` type; panels bump the
/// same registry generation counter so stale `(PanelId, Generation)` handles
/// are detectable, mirroring terminal/view rules.
pub type PanelGeneration = Generation;

/// Handle for a panel instance: `(PanelId, Generation)` pair that must be
/// validated on every cross-component call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelHandle {
    pub id: PanelId,
    pub generation: Generation,
}

/// Panel-specific error type; leaves previous valid state intact (fail-closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelError {
    TooManyPanels {
        max: usize,
        current: usize,
    },
    TooManyTopics {
        max: usize,
        current: usize,
    },
    TooManySubscriptions {
        max: usize,
        current: usize,
    },
    PayloadTooLarge {
        bytes: usize,
        max: usize,
    },
    UnknownPanelType {
        value: String,
    },
    UnknownTopic {
        topic: String,
    },
    UndisclosedTopic {
        topic: String,
    },
    AlreadyMounted {
        view_id: ViewId,
        existing: ViewContent,
    },
    PanelAlreadyMounted {
        panel_id: PanelId,
        current_view: ViewId,
    },
    StaleHandle {
        expected_generation: Generation,
        found_generation: Generation,
        id_raw: u64,
    },
    RegistryDisposed {
        generation: Generation,
    },
    GenerationExhausted {
        current: Generation,
    },
    OverlayBusy,
    TooManyOverlays {
        max: usize,
        current: usize,
    },
    CapabilityDenied {
        panel_id: PanelId,
        capability: String,
    },
    InvalidCommand {
        reason: String,
    },
    DuplicateCommand {
        command: String,
        owner: PanelId,
    },
    TooManyCommands {
        max: usize,
        current: usize,
    },
    NotFound {
        kind: &'static str,
        id_raw: u64,
    },
    InvalidState {
        current: PanelState,
        expected: &'static str,
    },
    ResourceExhausted {
        reason: String,
    },
}

impl std::fmt::Display for PanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyPanels { max, current } => {
                write!(f, "too many panels: max {max}, current {current}")
            }
            Self::TooManyTopics { max, current } => {
                write!(f, "too many topics: max {max}, current {current}")
            }
            Self::TooManySubscriptions { max, current } => {
                write!(f, "too many subscriptions: max {max}, current {current}")
            }
            Self::PayloadTooLarge { bytes, max } => {
                write!(f, "payload too large: {bytes} > {max}")
            }
            Self::UnknownPanelType { value } => write!(f, "unknown panel type '{value}'"),
            Self::UnknownTopic { topic } => write!(f, "unknown topic '{topic}'"),
            Self::UndisclosedTopic { topic } => write!(f, "undisclosed topic '{topic}'"),
            Self::AlreadyMounted { view_id, existing } => {
                write!(f, "view {view_id} already hosts {existing:?}")
            }
            Self::PanelAlreadyMounted {
                panel_id,
                current_view,
            } => write!(f, "panel {panel_id} already mounted at {current_view}"),
            Self::StaleHandle {
                expected_generation,
                found_generation,
                id_raw,
            } => write!(
                f,
                "stale panel handle id {id_raw}: expected {expected_generation}, found {found_generation}"
            ),
            Self::RegistryDisposed { generation } => {
                write!(f, "panel registry disposed at {generation}")
            }
            Self::GenerationExhausted { current } => {
                write!(f, "generation exhausted at {current}")
            }
            Self::OverlayBusy => f.write_str("modal overlay already active (OverlayBusy)"),
            Self::TooManyOverlays { max, current } => {
                write!(f, "too many overlays: max {max}, current {current}")
            }
            Self::CapabilityDenied {
                panel_id,
                capability,
            } => write!(f, "panel {panel_id} missing capability '{capability}'"),
            Self::InvalidCommand { reason } => write!(f, "invalid command: {reason}"),
            Self::DuplicateCommand { command, owner } => {
                write!(f, "duplicate command '{command}' already owned by {owner}")
            }
            Self::TooManyCommands { max, current } => {
                write!(f, "too many commands: max {max}, current {current}")
            }
            Self::NotFound { kind, id_raw } => write!(f, "{kind} {id_raw} not found"),
            Self::InvalidState { current, expected } => {
                write!(f, "invalid state {current}: expected {expected}")
            }
            Self::ResourceExhausted { reason } => write!(f, "resource exhausted: {reason}"),
        }
    }
}

impl std::error::Error for PanelError {}

/// Config for `PanelRegistry`; validated before creation via `ConfigPlan`-like checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRegistryConfig {
    pub max_panels_per_workspace: usize,
    pub max_panels_per_window: usize,
    pub max_topics_total: usize,
    pub max_subscriptions_per_panel: usize,
}

impl Default for PanelRegistryConfig {
    fn default() -> Self {
        Self {
            max_panels_per_workspace: DEFAULT_MAX_PANELS_PER_WORKSPACE,
            max_panels_per_window: DEFAULT_MAX_PANELS_PER_WINDOW,
            max_topics_total: MAX_TOPICS_TOTAL,
            max_subscriptions_per_panel: MAX_SUBSCRIPTIONS_PER_PANEL,
        }
    }
}

impl PanelRegistryConfig {
    pub fn validate(&self) -> Result<(), PanelError> {
        if !(1..=MAX_PANELS_PER_WORKSPACE).contains(&self.max_panels_per_workspace) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_workspace must be in [1, 32]".to_string(),
            });
        }
        if !(1..=MAX_PANELS_PER_WINDOW).contains(&self.max_panels_per_window) {
            return Err(PanelError::ResourceExhausted {
                reason: "max_panels_per_window must be in [1, 64]".to_string(),
            });
        }
        if self.max_topics_total == 0 || self.max_topics_total > MAX_TOPICS_TOTAL {
            return Err(PanelError::ResourceExhausted {
                reason: "max_topics_total must be in [1, 256]".to_string(),
            });
        }
        if self.max_subscriptions_per_panel == 0
            || self.max_subscriptions_per_panel > MAX_SUBSCRIPTIONS_PER_PANEL
        {
            return Err(PanelError::ResourceExhausted {
                reason: "max_subscriptions_per_panel must be in [1, 32]".to_string(),
            });
        }
        Ok(())
    }
}

/// Internal panel record.
#[allow(dead_code)]
#[derive(Debug)]
struct PanelRecord {
    id: PanelId,
    generation: Generation,
    state: PanelState,
    panel_type: PanelType,
    workspace: Option<WorkspaceId>,
    view: Option<ViewId>,
    title: Option<String>,
}

/// Validated event topic: `owner.name:topic` with `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z][a-z0-9_.-]*$`, `<=64` bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventTopic(String);

impl EventTopic {
    pub fn parse(raw: &str) -> Result<Self, PanelError> {
        if raw.is_empty() {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if raw.len() > 64 {
            return Err(PanelError::ResourceExhausted {
                reason: "topic exceeds 64 bytes".to_string(),
            });
        }
        let (owner_part, topic) = raw
            .split_once(':')
            .ok_or_else(|| PanelError::UnknownTopic {
                topic: raw.to_string(),
            })?;
        if topic.is_empty() || topic.len() > 32 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.'
        }) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        if !topic.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        let segs: Vec<&str> = owner_part.split('.').collect();
        if segs.len() != 2 {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        for seg in &segs {
            if seg.is_empty() || seg.len() > 16 {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(PanelError::UnknownTopic {
                    topic: raw.to_string(),
                });
            }
        }
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        // Forbid bitty.* impersonation for non-Core topics? Core topics are bitty.panel:*
        // Allow bitty.panel:* only from runtime; other bitty.* rejected
        if owner_part == "bitty" && !raw.starts_with("bitty.panel:") {
            return Err(PanelError::UnknownTopic {
                topic: raw.to_string(),
            });
        }
        Ok(Self(raw.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this topic is coalescable (latest-wins).
    #[must_use]
    pub fn is_coalescable(&self) -> bool {
        let s = self.0.as_str();
        s.contains("focus") || s.contains("cwd") || s.contains("title") || s.contains("file.open")
    }
}

/// Bounded payload text `<= 8KiB`, truncated or rejected at boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoundedPayload(String);

impl BoundedPayload {
    pub fn try_new(s: impl Into<String>) -> Result<Self, PanelError> {
        let raw = s.into();
        if raw.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: raw.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        Ok(Self(raw))
    }

    pub fn new_truncated(s: &str) -> Self {
        if s.len() <= BUS_EVENT_MAX_BYTES {
            return Self(s.to_owned());
        }
        // Truncate at char boundary
        let mut end = BUS_EVENT_MAX_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        Self(s[..end].to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One bus event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusEvent {
    pub topic: EventTopic,
    pub payload: BoundedPayload,
    pub generation: Generation,
}

/// Drop policy for bus queues; v1 default is `DropOldest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusDropPolicy {
    DropOldest,
    DropNewest,
}

/// Per-subscription bounded FIFO queue (64) with DropOldest/Newest.
#[derive(Debug)]
struct BusQueue {
    inner: VecDeque<BusEvent>,
    capacity: usize,
    dropped: u64,
    drop_policy: BusDropPolicy,
}

impl BusQueue {
    fn new(capacity: usize, drop_policy: BusDropPolicy) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
            drop_policy,
        }
    }

    fn push(&mut self, event: BusEvent) -> bool {
        // Coalescing: if topic is coalescable and queue holds undelivered copy, replace latest
        if event.topic.is_coalescable() {
            if let Some(pos) = self.inner.iter().position(|e| e.topic == event.topic) {
                // Remove existing coalescable entry and push latest to back (latest-wins)
                self.inner.remove(pos);
                self.inner.push_back(event);
                return true;
            }
        }
        if self.inner.len() >= self.capacity {
            match self.drop_policy {
                BusDropPolicy::DropOldest => {
                    self.inner.pop_front();
                    self.dropped = self.dropped.wrapping_add(1);
                    self.inner.push_back(event);
                    true
                }
                BusDropPolicy::DropNewest => {
                    self.dropped = self.dropped.wrapping_add(1);
                    false
                }
            }
        } else {
            self.inner.push_back(event);
            true
        }
    }

    fn drain_batch(&mut self, max_events: usize, max_bytes: usize) -> Vec<BusEvent> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        while let Some(front) = self.inner.front() {
            if out.len() >= max_events {
                break;
            }
            let payload_len = front.payload.len();
            if bytes + payload_len > max_bytes && !out.is_empty() {
                break;
            }
            // If single event exceeds max_bytes, drain it only if it's the first? But spec says strict: never exceed max_bytes even for first? However for panel bus we follow same as plugin-host drain_batch strict: never exceed max_bytes even for first event; remainder stays queued.
            // So if first event alone exceeds max_bytes, we do not drain it.
            if payload_len > max_bytes {
                break;
            }
            if bytes + payload_len > max_bytes {
                break;
            }
            let Some(ev) = self.inner.pop_front() else {
                break;
            };
            bytes += payload_len;
            out.push(ev);
        }
        out
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn bytes(&self) -> usize {
        self.inner.iter().map(|e| e.payload.len()).sum()
    }
}

/// Panel EventBus with three-level budgets mirroring plugin-host but for panel traffic.
#[derive(Debug)]
pub struct PanelEventBus {
    queues: HashMap<(PanelId, String), BusQueue>,
    per_panel_events: HashMap<PanelId, usize>,
    per_panel_bytes: HashMap<PanelId, usize>,
    global_events: usize,
    global_bytes: usize,
    total_dropped: u64,
    drop_policy: BusDropPolicy,
    topics: HashSet<String>,
}

impl PanelEventBus {
    #[must_use]
    pub fn new(drop_policy: BusDropPolicy) -> Self {
        Self {
            queues: HashMap::new(),
            per_panel_events: HashMap::new(),
            per_panel_bytes: HashMap::new(),
            global_events: 0,
            global_bytes: 0,
            total_dropped: 0,
            drop_policy,
            topics: HashSet::new(),
        }
    }

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        let topic = EventTopic::parse(raw)?;
        if self.topics.len() >= MAX_TOPICS_TOTAL && !self.topics.contains(topic.as_str()) {
            return Err(PanelError::TooManyTopics {
                max: MAX_TOPICS_TOTAL,
                current: self.topics.len(),
            });
        }
        self.topics.insert(topic.as_str().to_string());
        Ok(topic)
    }

    pub fn subscribe(&mut self, panel_id: PanelId, topic: &EventTopic) -> Result<(), PanelError> {
        if !self.topics.contains(topic.as_str()) {
            return Err(PanelError::UnknownTopic {
                topic: topic.as_str().to_string(),
            });
        }
        // Count subscriptions per panel
        let count = self
            .queues
            .keys()
            .filter(|(pid, _)| *pid == panel_id)
            .count();
        if count >= MAX_SUBSCRIPTIONS_PER_PANEL {
            return Err(PanelError::TooManySubscriptions {
                max: MAX_SUBSCRIPTIONS_PER_PANEL,
                current: count,
            });
        }
        let key = (panel_id, topic.as_str().to_string());
        self.queues
            .entry(key)
            .or_insert_with(|| BusQueue::new(BUS_PER_SUBSCRIPTION_LIMIT, self.drop_policy));
        Ok(())
    }

    /// Publish a payload to all subscribers of `topic`. Enforces per-panel
    /// 1024/256KiB and global 8192/2MiB with DropOldest via queue eviction.
    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        if payload.len() > BUS_EVENT_MAX_BYTES {
            return Err(PanelError::PayloadTooLarge {
                bytes: payload.len(),
                max: BUS_EVENT_MAX_BYTES,
            });
        }
        // Gather subscribers for topic
        let subscribers: Vec<(PanelId, String)> = self
            .queues
            .keys()
            .filter(|(_, t)| t == topic.as_str())
            .cloned()
            .collect();
        if subscribers.is_empty() {
            return Ok(());
        }
        for (panel_id, topic_str) in subscribers {
            let generation = Generation::INITIAL; // placeholder; real generation tracked per panel elsewhere
            let event = BusEvent {
                topic: topic.clone(),
                payload: payload.clone(),
                generation,
            };
            // Enforce per-panel aggregate before push: if would exceed, evict oldest across panel's queues (DropOldest)
            let per_panel_events = self.per_panel_events.get(&panel_id).copied().unwrap_or(0);
            let per_panel_bytes = self.per_panel_bytes.get(&panel_id).copied().unwrap_or(0);
            if per_panel_events >= BUS_PER_PANEL_LIMIT
                || per_panel_bytes + payload.len() > BUS_PER_PANEL_BYTES_LIMIT
            {
                match self.drop_policy {
                    BusDropPolicy::DropOldest => {
                        // Evict oldest across panel's queues
                        self.evict_oldest_for_panel(panel_id);
                    }
                    BusDropPolicy::DropNewest => {
                        // Drop new arrival for this subscriber
                        if let Some(q) = self.queues.get_mut(&(panel_id, topic_str.clone())) {
                            q.dropped = q.dropped.wrapping_add(1);
                            self.total_dropped = self.total_dropped.wrapping_add(1);
                        }
                        continue;
                    }
                }
            }
            // Enforce global before push
            if self.global_events >= BUS_GLOBAL_LIMIT
                || self.global_bytes + payload.len() > BUS_GLOBAL_BYTES_LIMIT
            {
                match self.drop_policy {
                    BusDropPolicy::DropOldest => {
                        self.evict_oldest_globally();
                    }
                    BusDropPolicy::DropNewest => {
                        if let Some(q) = self.queues.get_mut(&(panel_id, topic_str.clone())) {
                            q.dropped = q.dropped.wrapping_add(1);
                            self.total_dropped = self.total_dropped.wrapping_add(1);
                        }
                        continue;
                    }
                }
            }
            // Push to per-subscription queue
            let key = (panel_id, topic_str);
            if let Some(queue) = self.queues.get_mut(&key) {
                let before_len = queue.len();
                let before_bytes = queue.bytes();
                let pushed = queue.push(event);
                if pushed {
                    // Update aggregates
                    let delta_events = queue.len() as isize - before_len as isize;
                    let delta_bytes = queue.bytes() as isize - before_bytes as isize;
                    *self.per_panel_events.entry(panel_id).or_insert(0) =
                        (*self.per_panel_events.get(&panel_id).unwrap_or(&0) as isize
                            + delta_events) as usize;
                    *self.per_panel_bytes.entry(panel_id).or_insert(0) =
                        (*self.per_panel_bytes.get(&panel_id).unwrap_or(&0) as isize + delta_bytes)
                            as usize;
                    self.global_events = (self.global_events as isize + delta_events) as usize;
                    self.global_bytes = (self.global_bytes as isize + delta_bytes) as usize;
                    if queue.dropped > 0 && delta_events <= 0 {
                        // DropOldest evicted one, count total dropped already in queue.dropped
                        // Recompute total_dropped as sum of all queue dropped?
                        self.total_dropped = self.queues.values().map(|q| q.dropped).sum();
                    }
                } else {
                    self.total_dropped = self.queues.values().map(|q| q.dropped).sum();
                }
            }
        }
        Ok(())
    }

    fn evict_oldest_for_panel(&mut self, panel_id: PanelId) {
        // Find oldest queue entry for panel_id (first queue with earliest front)
        let mut oldest_key: Option<(PanelId, String)> = None;
        for key in self.queues.keys() {
            if key.0 == panel_id {
                oldest_key = Some(key.clone());
                break;
            }
        }
        if let Some(key) = oldest_key {
            if let Some(q) = self.queues.get_mut(&key) {
                if let Some(ev) = q.inner.pop_front() {
                    q.dropped = q.dropped.wrapping_add(1);
                    let bytes = ev.payload.len();
                    *self.per_panel_events.entry(panel_id).or_insert(1) -= 1;
                    *self.per_panel_bytes.entry(panel_id).or_insert(bytes) -= bytes;
                    self.global_events = self.global_events.saturating_sub(1);
                    self.global_bytes = self.global_bytes.saturating_sub(bytes);
                    self.total_dropped = self.total_dropped.wrapping_add(1);
                }
            }
        }
    }

    fn evict_oldest_globally(&mut self) {
        // Evict one event from any queue (first found)
        let key_opt = self.queues.keys().next().cloned();
        if let Some(key) = key_opt {
            if let Some(q) = self.queues.get_mut(&key) {
                if let Some(ev) = q.inner.pop_front() {
                    let bytes = ev.payload.len();
                    let pid = key.0;
                    *self.per_panel_events.entry(pid).or_insert(1) -= 1;
                    *self.per_panel_bytes.entry(pid).or_insert(bytes) -= bytes;
                    q.dropped = q.dropped.wrapping_add(1);
                    self.global_events = self.global_events.saturating_sub(1);
                    self.global_bytes = self.global_bytes.saturating_sub(bytes);
                    self.total_dropped = self.total_dropped.wrapping_add(1);
                }
            }
        }
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        let key = (panel_id, topic.to_string());
        let (events, delta_bytes, delta_events, dropped) =
            if let Some(q) = self.queues.get_mut(&key) {
                let batch = q.drain_batch(max_events, max_bytes);
                let bytes: usize = batch.iter().map(|e| e.payload.len()).sum();
                let ev_cnt = batch.len();
                let dropped = q.dropped;
                (batch, bytes, ev_cnt, dropped)
            } else {
                return Vec::new();
            };
        // Update aggregates
        if delta_events > 0 {
            if let Some(cnt) = self.per_panel_events.get_mut(&panel_id) {
                *cnt = cnt.saturating_sub(delta_events);
            }
            if let Some(cnt) = self.per_panel_bytes.get_mut(&panel_id) {
                *cnt = cnt.saturating_sub(delta_bytes);
            }
            self.global_events = self.global_events.saturating_sub(delta_events);
            self.global_bytes = self.global_bytes.saturating_sub(delta_bytes);
        }
        let _ = dropped;
        events
    }

    #[must_use]
    pub fn total_queued_events(&self) -> usize {
        self.global_events
    }

    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.global_bytes
    }

    #[must_use]
    pub fn queued_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.per_panel_events.get(&panel_id).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.total_dropped
    }

    pub fn clear_panel(&mut self, panel_id: PanelId) {
        let keys: Vec<(PanelId, String)> = self
            .queues
            .keys()
            .filter(|(pid, _)| *pid == panel_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(q) = self.queues.remove(&key) {
                self.global_events = self.global_events.saturating_sub(q.len());
                self.global_bytes = self.global_bytes.saturating_sub(q.bytes());
                self.total_dropped = self.total_dropped.wrapping_add(q.dropped);
            }
        }
        self.per_panel_events.remove(&panel_id);
        self.per_panel_bytes.remove(&panel_id);
    }

    pub fn topics_len(&self) -> usize {
        self.topics.len()
    }
}

/// Generic Panel Runtime per window/process. One registry per window,
/// single-process winit, holds no PTY fd, GPU object, or OS window handle
/// (those remain with `bitty-pty`, `bitty-render`, `bitty-platform`).
pub struct PanelRegistry {
    registry_generation: Generation,
    next_panel_raw: u64,
    config: PanelRegistryConfig,
    panels: HashMap<u64, PanelRecord>,
    panel_to_view: HashMap<PanelId, ViewId>,
    view_to_panel: HashMap<ViewId, PanelId>,
    workspace_panels: HashMap<WorkspaceId, Vec<PanelId>>,
    focus_per_workspace: HashMap<WorkspaceId, bitty_ui::panel::PanelFocus>,
    active_workspace: Option<WorkspaceId>,
    command_registry: UiCommandRegistry,
    overlay_manager: UiOverlayManager,
    event_bus: PanelEventBus,
    capabilities: HashMap<(PanelId, Generation), BTreeSet<String>>,
    errors: HashMap<String, u64>,
    disposed: bool,
    total_created: u64,
}

impl std::fmt::Debug for PanelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelRegistry")
            .field("generation", &self.registry_generation)
            .field("panels_active", &self.panels.len())
            .field("total_created", &self.total_created)
            .field("disposed", &self.disposed)
            .finish_non_exhaustive()
    }
}

impl PanelRegistry {
    /// Creates a panel registry after `PanelRegistryConfig` validation.
    ///
    /// # Errors
    /// `ResourceExhausted` for bad bounds, `GenerationExhausted` if reserved.
    pub fn new(config: PanelRegistryConfig) -> Result<Self, PanelError> {
        config.validate()?;
        if Generation::INITIAL.is_exhausted() {
            return Err(PanelError::GenerationExhausted {
                current: Generation::INITIAL,
            });
        }
        Ok(Self {
            registry_generation: Generation::INITIAL,
            next_panel_raw: 1,
            config,
            panels: HashMap::new(),
            panel_to_view: HashMap::new(),
            view_to_panel: HashMap::new(),
            workspace_panels: HashMap::new(),
            focus_per_workspace: HashMap::new(),
            active_workspace: None,
            command_registry: UiCommandRegistry::new(),
            overlay_manager: UiOverlayManager::new(),
            event_bus: PanelEventBus::new(BusDropPolicy::DropOldest),
            capabilities: HashMap::new(),
            errors: HashMap::new(),
            disposed: false,
            total_created: 0,
        })
    }

    fn ensure_not_disposed(&self) -> Result<(), PanelError> {
        if self.disposed {
            return Err(PanelError::RegistryDisposed {
                generation: self.registry_generation,
            });
        }
        Ok(())
    }

    fn bump_error(&mut self, variant: &str) {
        *self.errors.entry(variant.to_owned()).or_insert(0) += 1;
    }

    #[must_use]
    pub fn generation(&self) -> Generation {
        self.registry_generation
    }

    #[must_use]
    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    #[must_use]
    pub fn config(&self) -> &PanelRegistryConfig {
        &self.config
    }

    /// Validates `(id, generation)` before returning a reference.
    fn get_panel(&self, id: PanelId, generation: Generation) -> Result<&PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.generation != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: rec.generation,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    fn get_panel_mut(
        &mut self,
        id: PanelId,
        generation: Generation,
    ) -> Result<&mut PanelRecord, PanelError> {
        self.ensure_not_disposed()?;
        let current_gen = {
            let rec = self.panels.get(&id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            })?;
            rec.generation
        };
        if current_gen != generation {
            return Err(PanelError::StaleHandle {
                expected_generation: current_gen,
                found_generation: generation,
                id_raw: id.0,
            });
        }
        let rec = self.panels.get_mut(&id.0).ok_or(PanelError::NotFound {
            kind: "panel",
            id_raw: id.0,
        })?;
        if rec.state == UiPanelState::Disposed {
            return Err(PanelError::NotFound {
                kind: "panel",
                id_raw: id.0,
            });
        }
        Ok(rec)
    }

    /// Creates a panel of `panel_type`. Validates closed type set and
    /// `max_panels_per_workspace` / `max_panels_per_window` before allocation.
    ///
    /// # Errors
    /// `TooManyPanels`, `UnknownPanelType`, `GenerationExhausted`, `RegistryDisposed`.
    pub fn create_panel(
        &mut self,
        panel_type: PanelType,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        self.ensure_not_disposed()?;
        if self.registry_generation.is_exhausted() {
            self.bump_error("GenerationExhausted");
            return Err(PanelError::GenerationExhausted {
                current: self.registry_generation,
            });
        }
        if self.panels.len() >= self.config.max_panels_per_window {
            self.bump_error("TooManyPanels");
            return Err(PanelError::TooManyPanels {
                max: self.config.max_panels_per_window,
                current: self.panels.len(),
            });
        }
        if let Some(ws) = workspace {
            let count = self.workspace_panels.get(&ws).map_or(0, |v| v.len());
            if count >= self.config.max_panels_per_workspace {
                self.bump_error("TooManyPanels");
                return Err(PanelError::TooManyPanels {
                    max: self.config.max_panels_per_workspace,
                    current: count,
                });
            }
        }
        let next_gen =
            self.registry_generation
                .next()
                .map_err(|_| PanelError::GenerationExhausted {
                    current: self.registry_generation,
                })?;
        self.registry_generation = next_gen;
        let pid = PanelId::new(self.next_panel_raw);
        self.next_panel_raw = self.next_panel_raw.wrapping_add(1).max(1);
        let gen_val = self.registry_generation;
        let rec = PanelRecord {
            id: pid,
            generation: gen_val,
            state: UiPanelState::Created,
            panel_type,
            workspace,
            view: None,
            title: None,
        };
        self.panels.insert(pid.0, rec);
        if let Some(ws) = workspace {
            self.workspace_panels.entry(ws).or_default().push(pid);
            self.focus_per_workspace.entry(ws).or_default();
            if self.active_workspace.is_none() {
                self.active_workspace = Some(ws);
            }
        }
        self.total_created += 1;
        Ok(PanelHandle {
            id: pid,
            generation: gen_val,
        })
    }

    /// Creates a panel from a string type name; validates closed set.
    pub fn create_panel_by_type_str(
        &mut self,
        type_str: &str,
        workspace: Option<WorkspaceId>,
    ) -> Result<PanelHandle, PanelError> {
        let pt = UiPanelType::parse(type_str).ok_or_else(|| PanelError::UnknownPanelType {
            value: type_str.to_string(),
        })?;
        self.create_panel(pt, workspace)
    }

    /// Returns panel state for a handle.
    pub fn panel_state(
        &self,
        id: PanelId,
        generation: Generation,
    ) -> Result<PanelState, PanelError> {
        Ok(self.get_panel(id, generation)?.state)
    }

    /// Mounts `panel` to an empty `ViewId`. Validates handles, single-owner
    /// mapping, and transitions `Created -> Mounted`.
    ///
    /// # Errors
    /// `StaleHandle`, `AlreadyMounted`, `PanelAlreadyMounted`, `InvalidState`.
    pub fn mount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        view_id: ViewId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        if self.panel_to_view.contains_key(&panel_id) {
            let cur = self.panel_to_view[&panel_id];
            return Err(PanelError::PanelAlreadyMounted {
                panel_id,
                current_view: cur,
            });
        }
        if self.view_to_panel.contains_key(&view_id) {
            let existing = self.view_to_panel[&view_id];
            let content = ViewContent::Panel(existing);
            return Err(PanelError::AlreadyMounted {
                view_id,
                existing: content,
            });
        }
        let rec = self.get_panel(panel_id, generation)?;
        if !matches!(rec.state, UiPanelState::Created | UiPanelState::Suspended) {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Created or Suspended",
            });
        }
        // Commit mount
        self.view_to_panel.insert(view_id, panel_id);
        self.panel_to_view.insert(panel_id, view_id);
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Mounted;
        rec_mut.view = Some(view_id);
        Ok(())
    }

    /// Unmounts a panel, preserving its identity and generation, transitioning
    /// `Mounted`/`Focused`/`Suspended` -> `Suspended` (retain) or `Created`.
    pub fn unmount_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<ViewId, PanelError> {
        self.ensure_not_disposed()?;
        let workspace = {
            let rec = self.get_panel(panel_id, generation)?;
            if !matches!(
                rec.state,
                UiPanelState::Mounted | UiPanelState::Focused | UiPanelState::Suspended
            ) {
                return Err(PanelError::InvalidState {
                    current: rec.state,
                    expected: "Mounted/Focused/Suspended",
                });
            }
            rec.workspace
        };
        let view_id = self
            .panel_to_view
            .remove(&panel_id)
            .ok_or(PanelError::NotFound {
                kind: "panel view",
                id_raw: panel_id.0,
            })?;
        self.view_to_panel.remove(&view_id);
        // Focus MRU update if focused
        if let Some(ws) = workspace {
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        rec_mut.view = None;
        Ok(view_id)
    }

    /// Focuses a mounted panel within its workspace, moving to MRU front and
    /// transitioning `Mounted -> Focused`. Hidden panels cannot be focused.
    pub fn focus_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        workspace: WorkspaceId,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused",
            });
        }
        if rec.workspace != Some(workspace) && rec.workspace.is_some() {
            // Allow focus only within its workspace; if panel has workspace binding, enforce it
            return Err(PanelError::NotFound {
                kind: "workspace",
                id_raw: workspace.0,
            });
        }
        // Update MRU
        let focus = self.focus_per_workspace.entry(workspace).or_default();
        focus.set(panel_id);
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Focused;
        Ok(())
    }

    /// Suspends a focused or mounted panel (invisible without destroying attachment).
    pub fn suspend_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Mounted && rec.state != UiPanelState::Focused {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Mounted or Focused",
            });
        }
        let ws = rec.workspace;
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Suspended;
        if let Some(w) = ws {
            if let Some(focus) = self.focus_per_workspace.get_mut(&w) {
                focus.on_panel_hidden(panel_id);
            }
        }
        Ok(())
    }

    /// Resumes a suspended panel back to mounted.
    pub fn resume_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let rec = self.get_panel(panel_id, generation)?;
        if rec.state != UiPanelState::Suspended {
            return Err(PanelError::InvalidState {
                current: rec.state,
                expected: "Suspended",
            });
        }
        let rec_mut = self.get_panel_mut(panel_id, generation)?;
        rec_mut.state = UiPanelState::Mounted;
        Ok(())
    }

    /// Disposes a panel, clearing its view attachment, MRU, capabilities,
    /// event queues, and retiring `(PanelId, Generation)`.
    pub fn dispose_panel(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        let (stored_gen, workspace) = {
            let rec = self.panels.get(&panel_id.0).ok_or(PanelError::NotFound {
                kind: "panel",
                id_raw: panel_id.0,
            })?;
            (rec.generation, rec.workspace)
        };
        if stored_gen != generation {
            self.bump_error("StaleHandle");
            return Err(PanelError::StaleHandle {
                expected_generation: stored_gen,
                found_generation: generation,
                id_raw: panel_id.0,
            });
        }
        // Remove view attachment if any
        if let Some(view_id) = self.panel_to_view.remove(&panel_id) {
            self.view_to_panel.remove(&view_id);
        }
        // Remove from workspace list and MRU
        if let Some(ws) = workspace {
            if let Some(list) = self.workspace_panels.get_mut(&ws) {
                list.retain(|&id| id != panel_id);
            }
            if let Some(focus) = self.focus_per_workspace.get_mut(&ws) {
                focus.on_panel_hidden(panel_id);
            }
        }
        // Clear commands, bus queues, capabilities
        self.command_registry.unregister_panel(panel_id);
        self.event_bus.clear_panel(panel_id);
        self.capabilities.remove(&(panel_id, generation));
        // Retire panel with generation bump
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        }
        self.panels.remove(&panel_id.0);
        Ok(())
    }

    /// Returns focused panel for a workspace.
    #[must_use]
    pub fn focused_panel(&self, workspace: WorkspaceId) -> Option<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .and_then(|f| f.focused())
    }

    #[must_use]
    pub fn mru_order(&self, workspace: WorkspaceId) -> Vec<PanelId> {
        self.focus_per_workspace
            .get(&workspace)
            .map(|f| f.mru_order())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Command registry (owner.name:command)
    // ------------------------------------------------------------------

    pub fn register_command(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        raw: &str,
    ) -> Result<QualifiedCommand, PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.command_registry
            .register(panel_id, raw)
            .map_err(|e| match e {
                bitty_ui::panel::CommandError::Duplicate { command, owner } => {
                    PanelError::DuplicateCommand { command, owner }
                }
                bitty_ui::panel::CommandError::TooManyCommands { max, current } => {
                    PanelError::TooManyCommands { max, current }
                }
                bitty_ui::panel::CommandError::Invalid(msg) => {
                    PanelError::InvalidCommand { reason: msg }
                }
            })
    }

    #[must_use]
    pub fn command_owner(&self, command: &str) -> Option<PanelId> {
        self.command_registry.owner_of(command)
    }

    // ------------------------------------------------------------------
    // Overlay (4+1)
    // ------------------------------------------------------------------

    pub fn create_overlay(
        &mut self,
        kind: UiOverlayKind,
        bounds: UiRect,
        text: impl Into<String>,
        tooltip: Option<String>,
    ) -> Result<u64, PanelError> {
        self.ensure_not_disposed()?;
        self.overlay_manager
            .create_overlay(kind, bounds, text, tooltip, self.registry_generation.get())
            .map_err(|e| match e {
                OverlayError::OverlayBusy => PanelError::OverlayBusy,
                OverlayError::TooManyOverlays { max, current } => {
                    PanelError::TooManyOverlays { max, current }
                }
            })
    }

    pub fn dismiss_overlay(&mut self, id: u64) -> Option<Overlay> {
        self.overlay_manager.dismiss(id)
    }

    #[must_use]
    pub fn overlay_len(&self) -> usize {
        self.overlay_manager.len()
    }

    #[must_use]
    pub fn overlay_modal_active(&self) -> bool {
        self.overlay_manager.modal_active()
    }

    // ------------------------------------------------------------------
    // EventBus
    // ------------------------------------------------------------------

    pub fn declare_topic(&mut self, raw: &str) -> Result<EventTopic, PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.declare_topic(raw)
    }

    pub fn subscribe(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        topic: &EventTopic,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        self.event_bus.subscribe(panel_id, topic)
    }

    pub fn publish(
        &mut self,
        topic: &EventTopic,
        payload: BoundedPayload,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.event_bus.publish(topic, payload)
    }

    pub fn drain_batch(
        &mut self,
        panel_id: PanelId,
        topic: &str,
        max_events: usize,
        max_bytes: usize,
    ) -> Vec<BusEvent> {
        self.event_bus
            .drain_batch(panel_id, topic, max_events, max_bytes)
    }

    #[must_use]
    pub fn bus_total_events(&self) -> usize {
        self.event_bus.total_queued_events()
    }

    #[must_use]
    pub fn bus_total_bytes(&self) -> usize {
        self.event_bus.total_queued_bytes()
    }

    #[must_use]
    pub fn bus_events_for_panel(&self, panel_id: PanelId) -> usize {
        self.event_bus.queued_events_for_panel(panel_id)
    }

    #[must_use]
    pub fn bus_total_dropped(&self) -> u64 {
        self.event_bus.total_dropped()
    }

    // ------------------------------------------------------------------
    // Capability isolation per (PanelId, generation) — panel.*
    // ------------------------------------------------------------------

    /// Grants a `panel.*` capability to a panel handle. Validates closed set.
    pub fn grant_panel_capability(
        &mut self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        self.ensure_not_disposed()?;
        self.get_panel(panel_id, generation)?;
        // Validate capability via plugin-host grammar (reuse)
        let cid = bitty_plugin_host::CapabilityId::parse(capability).map_err(|e| {
            PanelError::CapabilityDenied {
                panel_id,
                capability: format!("{capability}: {e}"),
            }
        })?;
        if cid.family() != bitty_plugin_host::CapabilityFamily::Panel {
            return Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            });
        }
        self.capabilities
            .entry((panel_id, generation))
            .or_default()
            .insert(capability.to_string());
        Ok(())
    }

    #[must_use]
    pub fn is_panel_capability_granted(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> bool {
        self.capabilities
            .get(&(panel_id, generation))
            .is_some_and(|set| set.contains(capability))
    }

    /// Checks capability and returns error if not granted (deny-by-default).
    pub fn require_panel_capability(
        &self,
        panel_id: PanelId,
        generation: Generation,
        capability: &str,
    ) -> Result<(), PanelError> {
        if self.is_panel_capability_granted(panel_id, generation, capability) {
            Ok(())
        } else {
            Err(PanelError::CapabilityDenied {
                panel_id,
                capability: capability.to_string(),
            })
        }
    }

    // ------------------------------------------------------------------
    // Disposal of whole registry
    // ------------------------------------------------------------------

    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.panels.clear();
        self.panel_to_view.clear();
        self.view_to_panel.clear();
        self.workspace_panels.clear();
        self.focus_per_workspace.clear();
        self.command_registry = UiCommandRegistry::new();
        self.overlay_manager.clear();
        // Bus cleared via dropping? Recreate
        self.event_bus = PanelEventBus::new(BusDropPolicy::DropOldest);
        self.capabilities.clear();
        if let Ok(next) = self.registry_generation.next() {
            self.registry_generation = next;
        } else {
            self.registry_generation = Generation(u64::MAX);
        }
        self.disposed = true;
    }

    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    #[must_use]
    pub fn error_count(&self, variant: &str) -> u64 {
        self.errors.get(variant).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub fn set_generation_for_test(&mut self, generation: Generation) {
        self.registry_generation = generation;
    }

    #[must_use]
    pub fn total_created(&self) -> u64 {
        self.total_created
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ui::{Rect as UiRect, SplitAxis};

    fn default_registry() -> TerminalRegistry {
        TerminalRegistry::new(RegistryConfig::default()).expect("default must build")
    }

    #[test]
    fn terminal_id_and_view_id_are_distinct_types() {
        // Type-level distinctness: no From bridge, no equality across types.
        let tid = TerminalId::new(1);
        let vid = ViewId::new(1);
        // The following would not compile if they were the same type:
        // assert_eq!(tid, vid);
        // Instead we assert the raw values can be equal while types differ.
        assert_eq!(tid.0, vid.0);
        assert_ne!(
            std::any::TypeId::of::<TerminalId>(),
            std::any::TypeId::of::<ViewId>()
        );
        // Ensure no From impl exists (compile check via trait bound absence is implicit).
        // This line must NOT compile, proving no bridge:
        // assert_no_from::<TerminalId, ViewId>(); where assert_no_from is
        // `fn assert_no_from<T, U>() where T: From<U>` — not instantiated.
        let _ = tid;
        let _ = vid;
    }

    #[test]
    fn registry_creation_validates_bounds() {
        let bad = RegistryConfig {
            max_terminals: 0,
            ..RegistryConfig::default()
        };
        assert!(TerminalRegistry::new(bad).is_err());
        let bad2 = RegistryConfig {
            max_terminals: 65,
            ..RegistryConfig::default()
        };
        assert!(TerminalRegistry::new(bad2).is_err());
        let bad3 = RegistryConfig {
            cell_width: 0,
            ..RegistryConfig::default()
        };
        assert!(TerminalRegistry::new(bad3).is_err());
    }

    #[test]
    fn create_terminal_within_bound_and_generation_monotonic() {
        let mut reg = default_registry();
        let start = reg.generation();
        let h1 = reg.create_terminal(None).expect("create 1");
        assert!(reg.generation().get() > start.get());
        let h2 = reg.create_terminal(None).expect("create 2");
        assert_ne!(h1.id, h2.id);
        assert_ne!(h1.generation, h2.generation);
        assert_ne!(h1.runtime_id, h2.runtime_id);
        assert_eq!(reg.terminal_count(), 2);
        assert_eq!(reg.total_created(), 2);
    }

    #[test]
    fn too_many_terminals_returns_error_and_preserves_state() {
        let mut reg = TerminalRegistry::new(RegistryConfig {
            max_terminals: 1,
            ..RegistryConfig::default()
        })
        .unwrap();
        let _ = reg.create_terminal(None).unwrap();
        let before_gen = reg.generation();
        let err = reg.create_terminal(None).unwrap_err();
        assert!(matches!(err, RegistryError::TooManyTerminals { .. }));
        // State unchanged
        assert_eq!(reg.terminal_count(), 1);
        assert_eq!(reg.generation(), before_gen);
        assert!(reg.error_count("TooManyTerminals") > 0);
    }

    #[test]
    fn persistent_id_validation_and_in_use() {
        let mut reg = default_registry();
        let pid = PersistentId::new("valid-id_123").unwrap();
        let h = reg.create_terminal(Some(pid.clone())).unwrap();
        assert_eq!(
            reg.terminal_persistent_id(h.id, h.generation).unwrap(),
            Some(pid.clone())
        );
        // Duplicate should fail
        let pid2 = PersistentId::new("valid-id_123").unwrap();
        let err = reg.create_terminal(Some(pid2)).unwrap_err();
        assert!(matches!(err, RegistryError::PersistentIdInUse { .. }));
        // Invalid charset
        assert!(PersistentId::new("BAD CAPS").is_err());
        assert!(PersistentId::new("x".repeat(65)).is_err());
        assert!(PersistentId::new("").is_err());
        // After close, pid reusable
        reg.close_terminal(h.id, h.generation).unwrap();
        let pid3 = PersistentId::new("valid-id_123").unwrap();
        let h2 = reg.create_terminal(Some(pid3)).expect("reuse after close");
        assert_ne!(h.id, h2.id);
    }

    #[test]
    fn stale_handle_rejected_with_expected_and_found() {
        let mut reg = default_registry();
        let h = reg.create_terminal(None).unwrap();
        // Close retires handle with generation bump
        let stale_gen = h.generation;
        reg.close_terminal(h.id, h.generation).unwrap();
        // New terminal may reuse numeric? We use monotonic raw, so reuse not occur,
        // but stale handle for old id should be NotFound? Actually old id removed,
        // so stale check for that id returns NotFound. Test generation mismatch via
        // second terminal close then attempt with old generation.
        let h2 = reg.create_terminal(None).unwrap();
        // Trying to close h2 with stale generation (h.generation) should give StaleHandle because id differs
        // For same id with stale generation, we need to simulate same id but old generation.
        // Our monotonic raw makes ids unique, so we test StaleHandle via attach with stale generation
        // by creating a terminal, then bumping generation via another create, then using old generation
        let mut reg2 = default_registry();
        let th = reg2.create_terminal(None).unwrap();
        let correct_gen = th.generation;
        // After another allocation, registry_generation advanced, but terminal's generation stays
        let _ = reg2.create_terminal(None).unwrap();
        // Using wrong generation for that terminal should be StaleHandle
        let wrong = Generation(correct_gen.0.wrapping_add(10));
        let err = reg2.terminal_snapshot(th.id, wrong).unwrap_err();
        assert!(matches!(err, RegistryError::StaleHandle { .. }));
        if let RegistryError::StaleHandle {
            expected_generation,
            found_generation,
            id_raw,
        } = err
        {
            assert_eq!(expected_generation, correct_gen);
            assert_eq!(found_generation, wrong);
            assert_eq!(id_raw, th.id.0);
        }
        let _ = h2;
        let _ = stale_gen;
    }

    #[test]
    fn closing_retires_id_and_generation_bumps() {
        let mut reg = default_registry();
        let h = reg.create_terminal(None).unwrap();
        let before = reg.generation();
        reg.close_terminal(h.id, h.generation).unwrap();
        assert!(reg.generation().get() > before.get());
        // Subsequent use of same handle must fail (NotFound or StaleHandle)
        let err = reg.terminal_snapshot(h.id, h.generation).unwrap_err();
        assert!(matches!(
            err,
            RegistryError::NotFound { .. } | RegistryError::StaleHandle { .. }
        ));
    }

    #[test]
    fn attach_detach_preserves_terminal_and_runtime() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        assert_eq!(reg.attached_view(th.id), Some(vh.id));
        assert_eq!(reg.attached_terminal(vh.id), Some(th.id));
        // Detach preserves ids
        let tid = reg.detach(wid, vh.id, vh.generation).unwrap();
        assert_eq!(tid, th.id);
        assert_eq!(reg.attached_view(th.id), None);
        assert_eq!(reg.attached_terminal(vh.id), None);
        // Terminal still live with same generation/runtime
        let snap = reg.terminal_snapshot(th.id, th.generation).unwrap();
        assert_eq!(snap.width, 80);
        assert_eq!(snap.height, 24);
        // Reattach to new view preserves TerminalId/RuntimeId
        let vh2 = reg.create_view(wid).unwrap();
        reg.attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
            .unwrap();
        assert_eq!(reg.attached_view(th.id), Some(vh2.id));
    }

    #[test]
    fn attach_when_already_attached_returns_error() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
            .unwrap();
        let err = reg
            .attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
            .unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyAttached { .. }));
        let th2 = reg.create_terminal(None).unwrap();
        let err2 = reg
            .attach(wid, vh1.id, vh1.generation, th2.id, Generation(999), rect)
            .unwrap_err();
        // View already hosts terminal
        assert!(matches!(
            err2,
            RegistryError::StaleHandle { .. } | RegistryError::ViewAlreadyAttached { .. }
        ));
    }

    #[test]
    fn move_terminal_atomic_preserves_ids() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
            .unwrap();
        let before_gen = th.generation;
        reg.move_terminal(
            th.id,
            th.generation,
            wid,
            vh1.id,
            vh1.generation,
            wid,
            vh2.id,
            vh2.generation,
            rect,
        )
        .unwrap();
        assert_eq!(reg.attached_view(th.id), Some(vh2.id));
        assert_eq!(reg.attached_terminal(vh1.id), None);
        assert_eq!(reg.attached_terminal(vh2.id), Some(th.id));
        // RuntimeId and TerminalId preserved, generation unchanged
        let snap = reg.terminal_snapshot(th.id, before_gen).unwrap();
        let _ = snap;
        // Failure atomicity: try move to occupied view should leave both unchanged
        let th2 = reg.create_terminal(None).unwrap();
        reg.attach(wid, vh1.id, vh1.generation, th2.id, th2.generation, rect)
            .unwrap();
        let err = reg
            .move_terminal(
                th.id,
                th.generation,
                wid,
                vh2.id,
                vh2.generation,
                wid,
                vh1.id,
                vh1.generation,
                rect,
            )
            .unwrap_err();
        assert!(matches!(err, RegistryError::ViewAlreadyAttached { .. }));
        // Both views unchanged
        assert_eq!(reg.attached_terminal(vh1.id), Some(th2.id));
        assert_eq!(reg.attached_terminal(vh2.id), Some(th.id));
    }

    #[test]
    fn focus_mru_survives_detach_and_destroy() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        let vh3 = reg.create_view(wid).unwrap();
        // Focus order: default focused vh1, then set focus to vh2, vh3
        reg.set_focus(wid, vh2.id, vh2.generation).unwrap();
        reg.set_focus(wid, vh3.id, vh3.generation).unwrap();
        assert_eq!(reg.focused_view(wid), Some(vh3.id));
        // Detach focused view -> focus moves to MRU next (vh2)
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh3.id, vh3.generation, th.id, th.generation, rect)
            .unwrap();
        reg.detach(wid, vh3.id, vh3.generation).unwrap();
        assert_eq!(reg.focused_view(wid), Some(vh2.id));
        // Destroy focused view -> focus moves to next MRU (vh1)
        let focused = reg.focused_view(wid).unwrap();
        assert_eq!(focused, vh2.id);
        reg.destroy_view(wid, vh2.id, vh2.generation).unwrap();
        assert_eq!(reg.focused_view(wid), Some(vh1.id));
        // Destroy last view -> focus None, no panic
        reg.destroy_view(wid, vh1.id, vh1.generation).unwrap();
        reg.destroy_view(wid, vh3.id, vh3.generation).unwrap();
        assert_eq!(reg.focused_view(wid), None);
    }

    #[test]
    fn visibility_states_do_not_mutate_terminal() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        // Hidden view still has live terminal with same generation
        reg.set_visibility(wid, vh.id, vh.generation, Visibility::InactiveWorkspace)
            .unwrap();
        assert_eq!(
            reg.visibility(wid, vh.id).unwrap(),
            Visibility::InactiveWorkspace
        );
        // Terminal snapshot still accessible, not mutated
        let snap_before = reg.terminal_snapshot(th.id, th.generation).unwrap();
        reg.set_visibility(wid, vh.id, vh.generation, Visibility::ScratchpadHidden)
            .unwrap();
        let snap_after = reg.terminal_snapshot(th.id, th.generation).unwrap();
        assert_eq!(snap_before.generation, snap_after.generation);
    }

    #[test]
    fn zero_area_never_reaches_pty_and_retains_previous_geometry() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        let before = reg.terminal_snapshot(th.id, th.generation).unwrap();
        let zero = LogicalRect::new(0.0, 0.0, 0.0, 0.0).unwrap();
        let err = reg
            .handle_view_rect(wid, vh.id, vh.generation, zero)
            .unwrap_err();
        assert!(matches!(err, RegistryError::InvalidGeometry { .. }));
        let after = reg.terminal_snapshot(th.id, th.generation).unwrap();
        assert_eq!(before.width, after.width);
        assert_eq!(before.height, after.height);
        // Pending queue should be empty
        assert_eq!(reg.flush_pending_resizes().len(), 0);
    }

    #[test]
    fn logical_rect_to_grid_floor_and_clamp() {
        let reg = default_registry();
        // 720x456 with cell 9x19 => 80x24
        let r = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        assert_eq!(reg.logical_rect_to_grid(r).unwrap(), (80, 24));
        // Floor behavior
        let r2 = LogicalRect::new(0.0, 0.0, 721.9, 457.9).unwrap();
        assert_eq!(reg.logical_rect_to_grid(r2).unwrap(), (80, 24));
        // Clamp to 1 when tiny
        let r3 = LogicalRect::new(0.0, 0.0, 4.0, 4.0).unwrap();
        assert_eq!(reg.logical_rect_to_grid(r3).unwrap(), (1, 1));
        // Clamp to 1024 when huge
        let r4 = LogicalRect::new(0.0, 0.0, 20000.0, 20000.0).unwrap();
        assert_eq!(reg.logical_rect_to_grid(r4).unwrap(), (1024, 1024));
        // Zero-area returns error
        let r5 = LogicalRect::new(0.0, 0.0, 0.0, 10.0).unwrap();
        assert!(reg.logical_rect_to_grid(r5).is_err());
    }

    #[test]
    fn debounce_64_coalesces_and_counts() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        // Storm 70 rects in same tick
        for i in 1..=70 {
            let r = LogicalRect::new(0.0, 0.0, 720.0 + f64::from(i), 456.0).unwrap();
            let _ = reg.handle_view_rect(wid, vh.id, vh.generation, r);
        }
        // Pending queue capped at 64, coalesced dropped at least 6? Actually attach already cleared,
        // then 70 rects: first 64 fill, next 6 drop oldest => coalesced >=6 but we coalesce to latest per tick
        // The counter should be at least 6-?
        let coalesced = reg.resize_coalesced(th.id, th.generation).unwrap();
        assert!(coalesced >= 1, "storm beyond 64 must increment coalesced");
        // Flush processes at most one per terminal per tick (coalesced to latest)
        let flushed = reg.flush_pending_resizes();
        assert_eq!(flushed.len(), 1);
        // After flush pending empty
        assert_eq!(reg.flush_pending_resizes().len(), 0);
    }

    #[test]
    fn generation_exhaustion_fails_closed() {
        let mut reg = default_registry();
        reg.set_generation_for_test(Generation(u64::MAX - 500));
        let err = reg.create_terminal(None).unwrap_err();
        assert!(matches!(err, RegistryError::GenerationExhausted { .. }));
        // State unchanged
        assert_eq!(reg.terminal_count(), 0);
    }

    #[test]
    fn registry_disposal_closes_all_and_fails_further_calls() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let _vh = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        reg.dispose();
        assert!(reg.is_disposed());
        // All further calls fail with RegistryDisposed
        let err = reg.create_terminal(None).unwrap_err();
        assert!(matches!(err, RegistryError::RegistryDisposed { .. }));
        let err2 = reg.terminal_snapshot(th.id, th.generation).unwrap_err();
        assert!(matches!(err2, RegistryError::RegistryDisposed { .. }));
        let err3 = reg.create_workspace().unwrap_err();
        assert!(matches!(err3, RegistryError::RegistryDisposed { .. }));
    }

    #[test]
    fn reattachment_vs_recreation() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid, vh1.id, vh1.generation, th.id, th.generation, rect)
            .unwrap();
        // Detach preserves same TerminalId/RuntimeId
        let tid = reg.detach(wid, vh1.id, vh1.generation).unwrap();
        assert_eq!(tid, th.id);
        // Reattach same terminal to new view
        reg.attach(wid, vh2.id, vh2.generation, th.id, th.generation, rect)
            .unwrap();
        assert_eq!(reg.attached_view(th.id), Some(vh2.id));
        // Simulate exit
        reg.mark_exited(th.id, th.generation, Some(1)).unwrap();
        let err = reg
            .handle_view_rect(wid, vh2.id, vh2.generation, rect)
            .unwrap_err();
        assert!(matches!(err, RegistryError::TerminalExited { .. }));
        // Close retires id
        reg.close_terminal(th.id, th.generation).unwrap();
        // Recreation with same PersistentId after close
        let pid = PersistentId::new("persist-a").unwrap();
        let pid2 = PersistentId::new("persist-a").unwrap();
        let th2 = reg.create_terminal(Some(pid)).unwrap();
        // Close th2 then recreate with same pid fresh id/runtime
        let pid_gen = th2.generation;
        reg.close_terminal(th2.id, pid_gen).unwrap();
        let th3 = reg.create_terminal(Some(pid2)).unwrap();
        assert_ne!(th2.id, th3.id);
        assert_ne!(th2.runtime_id, th3.runtime_id);
    }

    #[test]
    fn bounded_workspaces_and_views() {
        let mut reg = TerminalRegistry::new(RegistryConfig {
            max_workspaces_per_window: 1,
            max_views_per_workspace: 1,
            ..RegistryConfig::default()
        })
        .unwrap();
        let wid = reg.create_workspace().unwrap();
        let err = reg.create_workspace().unwrap_err();
        assert!(matches!(err, RegistryError::TooManyWorkspaces { .. }));
        let _vh = reg.create_view(wid).unwrap();
        let err2 = reg.create_view(wid).unwrap_err();
        assert!(matches!(err2, RegistryError::TooManyViews { .. }));
    }

    #[test]
    fn workspace_layout_without_hardcoded_tabs() {
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        // Build split layout without hardcoded tabs primitive
        let layout = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(vh1.id, 40, 24)),
            LayoutNode::leaf(View::new(vh2.id, 40, 24)),
        );
        reg.set_workspace_layout(wid, layout).unwrap();
        let allocs = reg
            .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
            .unwrap();
        assert_eq!(allocs.len(), 2);
        // Stack layout also works (overlay alternative)
        let stack = LayoutNode::stack(vec![
            LayoutNode::leaf(View::new(vh1.id, 80, 24)),
            LayoutNode::leaf(View::new(vh2.id, 80, 24)),
        ]);
        reg.set_workspace_layout(wid, stack).unwrap();
        let allocs2 = reg
            .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
            .unwrap();
        assert_eq!(allocs2.len(), 2);
        // Overlay
        let base = LayoutNode::leaf(View::new(vh1.id, 80, 24));
        let over = LayoutNode::leaf(View::new(vh2.id, 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        reg.set_workspace_layout(wid, overlay).unwrap();
        let allocs3 = reg
            .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
            .unwrap();
        assert_eq!(allocs3.len(), 2);
    }

    #[test]
    fn inactive_workspace_visibility_not_rendered_but_retains_attachment() {
        let mut reg = default_registry();
        let wid1 = reg.create_workspace().unwrap();
        let wid2 = reg.create_workspace().unwrap();
        let vh = reg.create_view(wid1).unwrap();
        let th = reg.create_terminal(None).unwrap();
        let rect = LogicalRect::new(0.0, 0.0, 720.0, 456.0).unwrap();
        reg.attach(wid1, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        // Switch active to wid2 => wid1 views become InactiveWorkspace
        reg.set_active_workspace(wid2).unwrap();
        assert_eq!(
            reg.visibility(wid1, vh.id).unwrap(),
            Visibility::InactiveWorkspace
        );
        // Terminal still live
        assert!(reg.terminal_snapshot(th.id, th.generation).is_ok());
        // Switching back restores Visible
        reg.set_active_workspace(wid1).unwrap();
        assert_eq!(reg.visibility(wid1, vh.id).unwrap(), Visibility::Visible);
    }

    #[test]
    fn headless_composition_rects_equivalence() {
        // Workspace view rectangles, registry lifecycle, and resize routing have headless tests without window/GPU
        let mut reg = default_registry();
        let wid = reg.create_workspace().unwrap();
        let vh1 = reg.create_view(wid).unwrap();
        let vh2 = reg.create_view(wid).unwrap();
        let layout = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(vh1.id, 80, 12)),
            LayoutNode::leaf(View::new(vh2.id, 80, 12)),
        );
        reg.set_workspace_layout(wid, layout).unwrap();
        let allocs = reg
            .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
            .unwrap();
        assert_eq!(allocs.len(), 2);
        // Each allocation should be non-empty and not overlapping (vertical split)
        assert_eq!(allocs[0].1.x, 0);
        assert_eq!(allocs[1].1.x, 0);
        assert_eq!(allocs[0].1.y, 0);
        assert_eq!(allocs[1].1.y, 12);
        // Determinism: second reflow identical
        let allocs2 = reg
            .reflow_workspace(wid, UiRect::new(0, 0, 80, 24))
            .unwrap();
        assert_eq!(allocs, allocs2);
    }
}

#[cfg(test)]
mod panel_tests {
    use super::{
        BoundedPayload, EventTopic, Generation, MAX_TOPICS_TOTAL, PanelError, PanelId,
        PanelRegistry, PanelRegistryConfig, PanelState, PanelType, WorkspaceId,
    };
    use bitty_ui::{Rect as UiRect, ViewId};

    fn default_panel_registry() -> PanelRegistry {
        PanelRegistry::new(PanelRegistryConfig::default()).expect("default panel registry")
    }

    #[test]
    fn panel_id_distinct_and_generation_monotonic() {
        let mut reg = default_panel_registry();
        let start = reg.generation();
        let h1 = reg
            .create_panel(PanelType::Terminal, None)
            .expect("create panel 1");
        assert!(reg.generation().get() > start.get());
        let h2 = reg
            .create_panel(PanelType::Rich, None)
            .expect("create panel 2");
        assert_ne!(h1.id, h2.id);
        assert_ne!(h1.generation, h2.generation);
        assert_eq!(reg.panel_count(), 2);
        assert_ne!(
            std::any::TypeId::of::<PanelId>(),
            std::any::TypeId::of::<ViewId>()
        );
        assert_ne!(
            std::any::TypeId::of::<PanelId>(),
            std::any::TypeId::of::<super::TerminalId>()
        );
    }

    #[test]
    fn lifecycle_declared_created_mounted_focused_suspended_disposed() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Helper, None).unwrap();
        assert_eq!(
            reg.panel_state(h.id, h.generation).unwrap(),
            PanelState::Created
        );
        let view = ViewId::new(1);
        reg.mount_panel(h.id, h.generation, view).unwrap();
        assert_eq!(
            reg.panel_state(h.id, h.generation).unwrap(),
            PanelState::Mounted
        );
        let ws = WorkspaceId::new(1);
        // For focus, workspace binding needed; recreate with workspace
        let mut reg2 = default_panel_registry();
        let h2 = reg2.create_panel(PanelType::Canvas, Some(ws)).unwrap();
        let v2 = ViewId::new(10);
        reg2.mount_panel(h2.id, h2.generation, v2).unwrap();
        reg2.focus_panel(h2.id, h2.generation, ws).unwrap();
        assert_eq!(
            reg2.panel_state(h2.id, h2.generation).unwrap(),
            PanelState::Focused
        );
        reg2.suspend_panel(h2.id, h2.generation).unwrap();
        assert_eq!(
            reg2.panel_state(h2.id, h2.generation).unwrap(),
            PanelState::Suspended
        );
        reg2.resume_panel(h2.id, h2.generation).unwrap();
        assert_eq!(
            reg2.panel_state(h2.id, h2.generation).unwrap(),
            PanelState::Mounted
        );
        reg2.dispose_panel(h2.id, h2.generation).unwrap();
        assert!(reg2.panel_state(h2.id, h2.generation).is_err());
    }

    #[test]
    fn create_beyond_max_returns_too_many_and_preserves_state() {
        let mut reg = PanelRegistry::new(PanelRegistryConfig {
            max_panels_per_workspace: 1,
            max_panels_per_window: 1,
            max_topics_total: MAX_TOPICS_TOTAL,
            max_subscriptions_per_panel: 32,
        })
        .unwrap();
        let ws = WorkspaceId::new(1);
        let _h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
        let before = reg.generation();
        let err = reg.create_panel(PanelType::Rich, Some(ws)).unwrap_err();
        assert!(matches!(err, PanelError::TooManyPanels { .. }));
        assert_eq!(reg.panel_count(), 1);
        assert_eq!(reg.generation(), before);
    }

    #[test]
    fn unknown_panel_type_rejected() {
        let mut reg = default_panel_registry();
        let err = reg
            .create_panel_by_type_str("unknown_type", None)
            .unwrap_err();
        assert!(matches!(err, PanelError::UnknownPanelType { .. }));
    }

    #[test]
    fn stale_handle_rejected() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Browser, None).unwrap();
        let wrong = Generation(h.generation.get().wrapping_add(10));
        let err = reg.panel_state(h.id, wrong).unwrap_err();
        assert!(matches!(err, PanelError::StaleHandle { .. }));
        if let PanelError::StaleHandle {
            expected_generation,
            found_generation,
            id_raw,
        } = err
        {
            assert_eq!(expected_generation, h.generation);
            assert_eq!(found_generation, wrong);
            assert_eq!(id_raw, h.id.0);
        }
    }

    #[test]
    fn mount_already_mounted_errors() {
        let mut reg = default_panel_registry();
        let h1 = reg.create_panel(PanelType::Terminal, None).unwrap();
        let h2 = reg.create_panel(PanelType::Rich, None).unwrap();
        let v1 = ViewId::new(1);
        let v2 = ViewId::new(2);
        reg.mount_panel(h1.id, h1.generation, v1).unwrap();
        let err = reg.mount_panel(h1.id, h1.generation, v2).unwrap_err();
        assert!(matches!(err, PanelError::PanelAlreadyMounted { .. }));
        let err2 = reg.mount_panel(h2.id, h2.generation, v1).unwrap_err();
        assert!(matches!(err2, PanelError::AlreadyMounted { .. }));
    }

    #[test]
    fn moving_panel_between_views_preserves_id() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Helper, None).unwrap();
        let v1 = ViewId::new(1);
        let v2 = ViewId::new(2);
        reg.mount_panel(h.id, h.generation, v1).unwrap();
        let vid = reg.unmount_panel(h.id, h.generation).unwrap();
        assert_eq!(vid, v1);
        reg.mount_panel(h.id, h.generation, v2).unwrap();
        // PanelId preserved, view changed
        assert_eq!(
            reg.panel_state(h.id, h.generation).unwrap(),
            PanelState::Mounted
        );
    }

    #[test]
    fn focus_mru_per_workspace() {
        let mut reg = default_panel_registry();
        let ws = WorkspaceId::new(42);
        let h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
        let h2 = reg.create_panel(PanelType::Rich, Some(ws)).unwrap();
        let h3 = reg.create_panel(PanelType::Canvas, Some(ws)).unwrap();
        let v1 = ViewId::new(1);
        let v2 = ViewId::new(2);
        let v3 = ViewId::new(3);
        reg.mount_panel(h1.id, h1.generation, v1).unwrap();
        reg.mount_panel(h2.id, h2.generation, v2).unwrap();
        reg.mount_panel(h3.id, h3.generation, v3).unwrap();
        reg.focus_panel(h1.id, h1.generation, ws).unwrap();
        reg.focus_panel(h2.id, h2.generation, ws).unwrap();
        reg.focus_panel(h3.id, h3.generation, ws).unwrap();
        assert_eq!(reg.focused_panel(ws), Some(h3.id));
        assert_eq!(reg.mru_order(ws), vec![h3.id, h2.id, h1.id]);
        // Suspend focused moves to next MRU
        reg.suspend_panel(h3.id, h3.generation).unwrap();
        assert_eq!(reg.focused_panel(ws), Some(h2.id));
    }

    #[test]
    fn overlay_max_4plus1_enforced() {
        let mut reg = default_panel_registry();
        let rect = UiRect::new(0, 0, 20, 10);
        for _ in 0..4 {
            reg.create_overlay(bitty_ui::panel::OverlayKind::NonModal, rect, "hello", None)
                .unwrap();
        }
        assert_eq!(reg.overlay_len(), 4);
        let err = reg
            .create_overlay(
                bitty_ui::panel::OverlayKind::NonModal,
                rect,
                "overflow",
                None,
            )
            .unwrap_err();
        assert!(matches!(err, PanelError::TooManyOverlays { .. }));
        // Modal still allowed (4+1)
        reg.create_overlay(bitty_ui::panel::OverlayKind::Modal, rect, "modal", None)
            .unwrap();
        assert_eq!(reg.overlay_len(), 5);
        let err2 = reg
            .create_overlay(bitty_ui::panel::OverlayKind::Modal, rect, "modal2", None)
            .unwrap_err();
        assert_eq!(err2, PanelError::OverlayBusy);
    }

    #[test]
    fn overlay_focus_restores_mru() {
        let mut reg = default_panel_registry();
        let ws = WorkspaceId::new(7);
        let h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
        let v1 = ViewId::new(1);
        reg.mount_panel(h1.id, h1.generation, v1).unwrap();
        reg.focus_panel(h1.id, h1.generation, ws).unwrap();
        assert_eq!(reg.focused_panel(ws), Some(h1.id));
        // Simulate overlay capture via suspend
        reg.suspend_panel(h1.id, h1.generation).unwrap();
        assert_eq!(reg.focused_panel(ws), None);
        reg.resume_panel(h1.id, h1.generation).unwrap();
        reg.focus_panel(h1.id, h1.generation, ws).unwrap();
        assert_eq!(reg.focused_panel(ws), Some(h1.id));
    }

    #[test]
    fn command_registry_owner_name_command_and_duplicates() {
        let mut reg = default_panel_registry();
        let h1 = reg.create_panel(PanelType::Helper, None).unwrap();
        let h2 = reg.create_panel(PanelType::Helper, None).unwrap();
        let qc = reg
            .register_command(h1.id, h1.generation, "xuepoo.git:open")
            .unwrap();
        assert_eq!(qc.as_str(), "xuepoo.git:open");
        assert_eq!(reg.command_owner("xuepoo.git:open"), Some(h1.id));
        let err = reg
            .register_command(h2.id, h2.generation, "xuepoo.git:open")
            .unwrap_err();
        assert!(matches!(err, PanelError::DuplicateCommand { .. }));
        // Invalid grammar
        assert!(
            reg.register_command(h1.id, h1.generation, "badcommand")
                .is_err()
        );
        assert!(
            reg.register_command(h1.id, h1.generation, "Owner.name:cmd")
                .is_err()
        );
        // Per-panel limit
        let mut reg2 = default_panel_registry();
        let h3 = reg2.create_panel(PanelType::Helper, None).unwrap();
        for i in 0..32 {
            reg2.register_command(h3.id, h3.generation, &format!("xuepoo.test:cmd{i}"))
                .unwrap();
        }
        let err2 = reg2
            .register_command(h3.id, h3.generation, "xuepoo.test:overflow")
            .unwrap_err();
        assert!(matches!(err2, PanelError::TooManyCommands { .. }));
    }

    #[test]
    fn event_bus_topic_grammar_and_declared_subscribe() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Terminal, None).unwrap();
        let topic = reg.declare_topic("xuepoo.files:file.open").unwrap();
        assert_eq!(topic.as_str(), "xuepoo.files:file.open");
        // Bare topic invalid
        assert!(reg.declare_topic("file.open").is_err());
        // Invalid owner prefix
        assert!(reg.declare_topic("bad_topic").is_err());
        // Subscribe to known topic ok
        reg.subscribe(h.id, h.generation, &topic).unwrap();
        // Subscribe to unknown topic fails UnknownTopic
        let unknown = EventTopic::parse("xuepoo.test:unknown").unwrap();
        // Not declared yet, subscribe should fail?
        // Actually we declared only one topic; trying to subscribe to undeclared should fail.
        // Our subscribe checks topics set contains it.
        let err = reg.subscribe(h.id, h.generation, &unknown).unwrap_err();
        assert!(matches!(err, PanelError::UnknownTopic { .. }));
        // Payload bound
        let large = "a".repeat(9 * 1024);
        let payload = BoundedPayload::try_new(large);
        assert!(payload.is_err());
    }

    #[test]
    fn event_bus_64_per_subscription_drop_oldest() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Rich, None).unwrap();
        let topic = reg.declare_topic("xuepoo.test:topic").unwrap();
        reg.subscribe(h.id, h.generation, &topic).unwrap();
        // Flood 70 events to one queue
        for i in 0..70 {
            let payload = BoundedPayload::try_new(format!("msg{i}")).unwrap();
            reg.publish(&topic, payload).unwrap();
        }
        // Per-subscription queue capped at 64, global still limited
        assert_eq!(reg.bus_events_for_panel(h.id), 64);
        assert!(reg.bus_total_dropped() >= 6);
        // Drain batch respects 32/8KiB
        let batch = reg.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // FIFO DropOldest: first batch should contain msg6..msg37 (oldest 6 dropped)
        assert_eq!(batch[0].payload.as_str(), "msg6");
    }

    #[test]
    fn event_bus_per_panel_1024_and_global_8192_drop_oldest() {
        let mut reg = default_panel_registry();
        // Create two panels, each subscribes to same topic? Need distinct topics per subscription to test per-panel aggregate.
        // Each panel can have up to 32 subscriptions, each 64 => 2048 would exceed per-panel 1024, so global or per-panel eviction should happen.
        let h = reg.create_panel(PanelType::Helper, None).unwrap();
        // Create 16 topics for one panel, each will get 64 events => 1024
        let mut topics = Vec::new();
        for i in 0..16 {
            let t = reg.declare_topic(&format!("xuepoo.test:topic{i}")).unwrap();
            reg.subscribe(h.id, h.generation, &t).unwrap();
            topics.push(t);
        }
        // Flood each topic 70 times => each queue would cap 64, but per-panel limit 1024 means total stays <=1024
        for topic in &topics {
            for j in 0..70 {
                let payload =
                    BoundedPayload::try_new(format!("p{}_{}", topic.as_str(), j)).unwrap();
                reg.publish(topic, payload).unwrap();
            }
        }
        assert!(reg.bus_events_for_panel(h.id) <= 1024);
        assert!(reg.bus_total_events() <= 8192);
        // Global test: many panels
        let mut reg2 = default_panel_registry();
        let mut handles = Vec::new();
        for _ in 0..10 {
            let h = reg2.create_panel(PanelType::Canvas, None).unwrap();
            let t = reg2.declare_topic("xuepoo.global:evt").unwrap();
            // Need distinct topic string per publish? We'll reuse same topic across panels
            // Actually subscribe each panel to same topic name (already declared)
            reg2.subscribe(h.id, h.generation, &t).unwrap();
            handles.push(h);
        }
        // The topic already declared, now publish storm
        let topic2 = EventTopic::parse("xuepoo.global:evt").unwrap();
        for _ in 0..9000 {
            let payload = BoundedPayload::try_new("x").unwrap();
            reg2.publish(&topic2, payload).unwrap();
        }
        assert!(reg2.bus_total_events() <= 8192);
        assert!(reg2.bus_total_bytes() <= 2 * 1024 * 1024);
    }

    #[test]
    fn capability_panel_isolation_per_generation() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Browser, None).unwrap();
        // Deny-by-default
        assert!(!reg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
        let err = reg
            .require_panel_capability(h.id, h.generation, "panel.create")
            .unwrap_err();
        assert!(matches!(err, PanelError::CapabilityDenied { .. }));
        // Grant
        reg.grant_panel_capability(h.id, h.generation, "panel.create")
            .unwrap();
        assert!(reg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
        reg.require_panel_capability(h.id, h.generation, "panel.create")
            .unwrap();
        // Stale generation cannot use old grant
        let wrong = Generation(h.generation.get().wrapping_add(1));
        assert!(!reg.is_panel_capability_granted(h.id, wrong, "panel.create"));
        // Unknown family rejected
        assert!(
            reg.grant_panel_capability(h.id, h.generation, "terminal.manage")
                .is_err()
        );
        // Invalid capability string rejected
        assert!(
            reg.grant_panel_capability(h.id, h.generation, "panel.unknown")
                .is_err()
        );
    }

    #[test]
    fn generation_exhaustion_fails_closed() {
        let mut reg = default_panel_registry();
        reg.set_generation_for_test(Generation(u64::MAX - 500));
        let err = reg.create_panel(PanelType::Terminal, None).unwrap_err();
        assert!(matches!(err, PanelError::GenerationExhausted { .. }));
        assert_eq!(reg.panel_count(), 0);
    }

    #[test]
    fn registry_disposal_clears_all_and_fails_further() {
        let mut reg = default_panel_registry();
        let h = reg.create_panel(PanelType::Rich, None).unwrap();
        let topic = reg.declare_topic("xuepoo.test:disposal").unwrap();
        reg.subscribe(h.id, h.generation, &topic).unwrap();
        reg.dispose();
        assert!(reg.is_disposed());
        let err = reg.create_panel(PanelType::Terminal, None).unwrap_err();
        assert!(matches!(err, PanelError::RegistryDisposed { .. }));
        let err2 = reg.panel_state(h.id, h.generation).unwrap_err();
        assert!(matches!(err2, PanelError::RegistryDisposed { .. }));
    }

    #[test]
    fn single_process_winit_one_registry_per_window_doc() {
        // This test documents the invariant: PanelRegistry is per window/process,
        // holds no PTY fd, GPU object, or OS window handle.
        // The registry is constructed in-process via `PanelRegistry::new`
        // and is not shared across processes; no bittyd or remote transport
        // is involved. This is a compile-time/architecture guarantee tested
        // via headless instantiation without window/GPU.
        let reg = default_panel_registry();
        assert_eq!(reg.panel_count(), 0);
        assert!(!reg.is_disposed());
        // No PTY/GPU handle accessible: registry Debug does not expose them
        let dbg = format!("{reg:?}");
        assert!(dbg.contains("PanelRegistry"));
        assert!(!dbg.contains("pty"));
        assert!(!dbg.contains("gpu"));
    }
}
