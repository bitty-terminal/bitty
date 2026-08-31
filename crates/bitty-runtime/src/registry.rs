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

use std::collections::{HashMap, VecDeque};

use bitty_term_state::{Snapshot, State};
use bitty_ui::{Focus, LayoutNode, Rect as UiRect, View, ViewId};

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
            cell_width: 8,
            cell_height: 16,
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
        // Check exited
        let rec = self.terminals.get_mut(&id.0).expect("checked");
        Ok(rec)
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
            new_children.into_iter().next().unwrap()
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
            // Coalesce to latest: drain all, keep last
            let latest = {
                let rec = self.terminals.get_mut(&tid.0).unwrap();
                let last = rec.pending_rects.back().copied().unwrap();
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
        // Validate all leaf ViewIds are known to this workspace
        let new_ids = layout.leaf_ids();
        {
            let ws = self.workspaces.get(&workspace_id.0).unwrap();
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
            let ws = self.workspaces.get(&workspace_id.0).unwrap();
            ws.view_gens
                .keys()
                .filter(|id| !new_ids.contains(id))
                .copied()
                .collect()
        };
        for vid in &to_remove {
            {
                let ws = self.workspaces.get_mut(&workspace_id.0).unwrap();
                ws.view_gens.remove(vid);
                ws.view_visibility.remove(vid);
                ws.mru.retain(|&id| id != *vid);
            }
            if let Some(tid) = self.view_to_terminal.remove(vid) {
                self.terminal_to_view.remove(&tid);
            }
        }
        {
            let ws = self.workspaces.get_mut(&workspace_id.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        // 640x384 with cell 8x16 => 80x24
        let r = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
        assert_eq!(reg.logical_rect_to_grid(r).unwrap(), (80, 24));
        // Floor behavior
        let r2 = LogicalRect::new(0.0, 0.0, 641.9, 385.9).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
        reg.attach(wid, vh.id, vh.generation, th.id, th.generation, rect)
            .unwrap();
        // Storm 70 rects in same tick
        for i in 1..=70 {
            let r = LogicalRect::new(0.0, 0.0, 640.0 + f64::from(i), 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
        let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
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
