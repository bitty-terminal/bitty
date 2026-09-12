//! `registry` — `TerminalRegistry` and terminal/workspace/view lifecycle.
//!
//! Split from `super` (`registry.rs`) as a pure move under CTX-0308:
//! byte-identical logic, only module wiring changed.

use super::*;

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
    pub(super) terminals: HashMap<u64, TerminalRecord>,
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
        let cols: u16 = bitty_term_state::GRID_COLUMNS as u16;
        let rows: u16 = bitty_term_state::GRID_ROWS as u16;
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
        // CTX-0364: creating a view/window focuses it immediately
        // (kitty/ghostty parity). The old rule only focused on the first
        // create in an empty workspace, so a second/third window left focus
        // on the previous one.
        ws.focus.set(vid);
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
        self.reflow_workspace_with_gaps(workspace_id, container, Gaps::ZERO)
    }

    /// Gap-aware workspace reflow (CTX-0177): like [`Self::reflow_workspace`]
    /// but allocates with `LayoutNode::layout_with_gaps`. With [`Gaps::ZERO`]
    /// this is identical to [`Self::reflow_workspace`].
    pub fn reflow_workspace_with_gaps(
        &mut self,
        workspace_id: WorkspaceId,
        container: UiRect,
        gaps: Gaps,
    ) -> Result<Vec<(ViewId, UiRect)>, RegistryError> {
        self.ensure_not_disposed()?;
        let ws = self.get_workspace_mut(workspace_id)?;
        ws.layout.reflow_with_gaps(container, gaps);
        Ok(ws.layout.layout_with_gaps(container, gaps))
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
