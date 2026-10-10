//! `Runtime` — Per-leaf pane sessions (spawn, close, pump, sync).
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
use std::path::PathBuf;

use super::layout_focus::PresentFrame;
use super::*;

/// One split pane's private shell session (CTX-0176).
///
/// Each leaf created by a split owns at most one of these: its own VT
/// parser, terminal grid state, query-overlap tail, and PTY triple. The
/// owned [`Pty`] keeps the child alive — dropping the session kills and
/// reaps it without leaking a zombie. The reader starts direct (drained by
/// [`Runtime::poll_pty`]) and is promoted into a wakeup forwarder by
/// [`Runtime::set_pty_waker`](super::Runtime::set_pty_waker), exactly like
/// the primary reader, so pane-only output wakes the event loop (CTX-0230).
pub(super) struct PaneSession {
    pub(super) parser: Parser,
    pub(super) state: State,
    pub(super) query_overlap: Vec<u8>,
    pub(super) pty: Pty,
    pub(super) reader: Option<PtyReader>,
    pub(super) forward_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    pub(super) forward_handle: Option<std::thread::JoinHandle<()>>,
    pub(super) writer: PtyWriter,
    /// Generation of `state` consumed by the last presented frame (CTX-0386).
    ///
    /// Per-pane damage is computed from this session's own ring
    /// (`state.damage_since(last_presented_generation)`), so a split pane
    /// that produced no output is reused from its retained leaf list instead
    /// of re-rendering. Initialized to `u64::MAX` ("never presented") so a
    /// freshly spawned session always reads as changed to frame-on-demand,
    /// matching the runtime-global primary sentinel.
    pub(super) last_presented_generation: u64,
}

impl PaneSession {
    /// Next immediately available chunk: the wakeup-forwarder channel when
    /// promoted, otherwise the direct pump channel. Either way the bound
    /// holds (`CHANNEL_CAPACITY_CHUNKS` x `READ_CHUNK_SIZE` per stage).
    pub(super) fn try_next_chunk(&self) -> Option<Vec<u8>> {
        if let Some(rx) = self.forward_rx.as_ref() {
            return rx.try_recv().ok();
        }
        match self.reader.as_ref()?.try_recv() {
            bitty_pty::PtyRecv::Chunk(chunk) => Some(chunk),
            bitty_pty::PtyRecv::Empty | bitty_pty::PtyRecv::Eof | bitty_pty::PtyRecv::Error(_) => {
                None
            }
        }
    }
}

impl Runtime {
    /// Spawns `program` inside a PTY sized to the current grid, storing the
    /// child handle. The program is taken as a direct argv[0] without shell
    /// interpolation (P0 security posture).
    ///
    /// Replaces any previously spawned child: the old `Pty` is dropped, which
    /// kills and reaps its child without leaking a zombie. The output side
    /// is pumped into a bounded channel (`READ_CHUNK_SIZE` ×
    /// `CHANNEL_CAPACITY_CHUNKS` = 128 KiB) so backpressure is end-to-end;
    /// see [`poll_pty`] for the non-blocking drain that feeds
    /// [`handle_pty_bytes`].
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `program` is blank;
    /// [`RuntimeError::Pty`] when the platform reports spawn failure
    /// (`Unsupported` on Windows before the ConPTY slice, `Upstream` or
    /// `Io` elsewhere).
    pub fn spawn_shell(&mut self, program: &str) -> Result<(), RuntimeError> {
        self.spawn_shell_with_args(program, &[])
    }

    /// Spawns `program` with additional `args` inside a PTY sized to the
    /// current grid.
    ///
    /// Direct argv exec, no shell interpolation: `program` plus `args` are
    /// passed verbatim to the platform exec path. For a shell echo, pass
    /// `program = "/bin/sh"` and `args = &["-c", "echo hello"]`. Bounded
    /// backpressure and lifecycle are identical to [`spawn_shell`].
    pub fn spawn_shell_with_args(
        &mut self,
        program: &str,
        args: &[&str],
    ) -> Result<(), RuntimeError> {
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        // CTX-0343: binding the primary shell turns the focused leaf into a
        // `terminal`-content `View`; a `ws:`/`view:` entry that first matches
        // here must not compose a violating pair. Fail closed before spawn.
        if let Some(primary) = self.focus.focused() {
            let label = self.active_workspace_label();
            self.validate_view_target_at("terminal", label, primary)?;
        }
        // CTX-1080: declarative panel rules evaluated deterministically at
        // spawn from already-available metadata only (program plus argv,
        // empty title, terminal content). First match wins; empty rules mean
        // no override. Invalid patterns never match (fail-closed skip).
        // Same evaluation as the per-view path below so a startup-command
        // rule applies to the primary pane too.
        let matched_rule = {
            let cmd_line = bitty_config::panel_rules::command_line_for(program, args);
            bitty_config::panel_rules::find_match(
                &self.panel_spawn_rules,
                &cmd_line,
                "",
                "terminal",
            )
            .map(|(_, rule)| rule.clone())
        };
        let mut cols = self.cols.min(u16::MAX as usize) as u16;
        let mut rows = self.rows.min(u16::MAX as usize) as u16;
        if let Some(rule) = matched_rule.as_ref() {
            if let Some(w) = rule.width {
                cols = w.max(1);
            }
            if let Some(h) = rule.height {
                rows = h.max(1);
            }
        }
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        // CTX-0585 (M1-25): the primary restart attach must seed its cwd from
        // the captured `OSC 7` report, exactly like a split spawn. The leaf
        // focused at attach time becomes the owner below, so resolve against
        // it; `session_pending_cwd` fails open when there is no capture.
        if let Some(owner) = self.focus.focused() {
            if let Some(cwd) = self.session_pending_cwd(&owner) {
                builder = builder.cwd(cwd);
            }
        }
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // Replace any previously spawned child (drop kills old). A prior
        // wakeup forwarder (if any) is joined with a bound (CTX-0472):
        // drop its receiver so `send` fails fast, drop the old `Pty`
        // (EOF unblocks the pump, which unblocks the forwarder's `recv`),
        // then join with `FORWARDER_JOIN_TIMEOUT` instead of detaching.
        // A timeout detaches (thread exits on EOF/disconnect alone) but
        // never hangs respawn on a wedged child.
        let old_handle = self.pty_forward_handle.take();
        self.pty_forward_rx = None;
        self.pty = Some(pty);
        if let Some(handle) = old_handle {
            let _ = super::join_forwarder_with_timeout(handle, super::FORWARDER_JOIN_TIMEOUT);
        }
        self.pty_reader = Some(reader);
        self.pty_writer = Some(writer);
        // CTX-0359: the primary shell is painted and typed into through the
        // leaf focused at attach time (startup `--focus` included), so pin
        // that leaf as the primary owner and remember the exact program
        // recipe for `workspace_new` shell replay.
        self.primary_view = self.focus.focused();
        self.primary_spawn = Some((
            program.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
        ));
        // CTX-0803 (#1476): primary ownership may just have moved to the
        // focused leaf. A selection owned by the previous owner now resolves
        // to no grid, so it is dropped here rather than left for the read
        // guard.
        self.invalidate_stale_view_bindings();
        // If a waker is already installed (respawn after `set_pty_waker`),
        // promote immediately so the new child wakes the loop too.
        if self.pty_waker.is_some() {
            self.promote_pty_reader_to_forwarder();
        }
        // Clear pending input on new shell: fresh session, no stale keystrokes.
        self.pending_input.clear();
        self.pending_input_dropped = 0;
        // CTX-1011 (#1775): a primary (re)spawn retires the previous primary
        // session, so its buffered Kitty `OSC 99` groups die with it. A later
        // primary session can never complete stale primary text.
        self.kitty_assembler
            .discard_stream(super::bell::KittyStreamId::Primary);
        // CTX-0393: hydrate captured scrollback into the primary grid when
        // this spawn fulfils a restored session (no-op otherwise).
        if let Some(owner) = self.primary_view {
            let _ = self.hydrate_session_pending_for(owner);
            // CTX-0585: the captured primary cwd is consumed exactly once by
            // this attach (split panes drain through the pending map).
            if self.session_primary_cwd.as_ref().map(|(o, _)| *o) == Some(owner) {
                self.session_primary_cwd = None;
            }
        }
        // CTX-1080: apply the matched rule to the primary owner with the
        // same semantics as the per-view path below. Dims are a spawn-time
        // initial-size request (solver owns steady-state geometry, see the
        // per-view application); presentation stamps the owner leaf
        // verbatim; workspace moves only when the owner is focused and the
        // target exists, otherwise skipped fail-closed.
        if let Some(rule) = matched_rule.as_ref() {
            if rule.width.is_some() || rule.height.is_some() {
                self.state.resize(cols as usize, rows as usize);
            }
            if let Some(owner) = self.primary_view {
                if let Some(presentation) = rule.presentation {
                    let mode = match presentation {
                        bitty_config::panel_rules::PanelPresentation::Tiled => {
                            bitty_ui::presentation::PresentationMode::Tiled
                        }
                        bitty_config::panel_rules::PanelPresentation::Floating => {
                            bitty_ui::presentation::PresentationMode::Floating
                        }
                        bitty_config::panel_rules::PanelPresentation::Scratchpad => {
                            bitty_ui::presentation::PresentationMode::Scratchpad
                        }
                    };
                    if let Some(leaf) = self.layout.find_leaf_mut(owner) {
                        leaf.set_presentation(mode);
                    }
                    for slot in &mut self.workspaces {
                        if let Some(leaf) = slot.layout.find_leaf_mut(owner) {
                            leaf.set_presentation(mode);
                        }
                    }
                }
                if rule.width.is_some() || rule.height.is_some() {
                    let w = rule.width.unwrap_or(cols).max(1) as usize;
                    let h = rule.height.unwrap_or(rows).max(1) as usize;
                    if let Some(leaf) = self.layout.find_leaf_mut(owner) {
                        leaf.resize(w, h);
                    }
                }
                if let Some(target) = rule.workspace {
                    let target_idx = (target as usize).saturating_sub(1);
                    if self.focus.focused() == Some(owner)
                        && target_idx < self.workspaces.len()
                        && target_idx != self.active_workspace_index()
                    {
                        let _ = self.workspace_move_focused_to(target_idx);
                    }
                }
            }
        }
        Ok(())
    }

    /// Program + args the primary shell last attached with, when it succeeded
    /// (CTX-0359).
    ///
    /// Creation paths that give a fresh leaf its own shell replay this recipe
    /// so a new pane starts exactly the program the primary attached with
    /// (`workspace_new`, the ctl split/spawn verbs; keymap parity).
    /// `None` before any successful primary attach (headless runtimes,
    /// startup spawn failure), where callers supply their own default
    /// resolution.
    #[must_use]
    pub fn primary_spawn_recipe(&self) -> Option<(&str, &[String])> {
        self.primary_spawn
            .as_ref()
            .map(|(program, args)| (program.as_str(), args.as_slice()))
    }

    /// Resolves the working directory a shell spawned as leaf `target` should
    /// inherit from the previously focused (source) pane.
    ///
    /// Mirrors kitty `launch --cwd=current` and ghostty
    /// `split-inherit-working-directory=true`: the new pane starts in the
    /// directory most recently reported by the focused surface over `OSC 7`
    /// (CTX-0357). The focused view is a source only while it is a different
    /// view than the one being spawned — replacing a session never seeds the
    /// fresh shell from the report of the session it replaces.
    ///
    /// Fail-open by construction: `None` when there is no focused source, no
    /// report, the report is not a `file://` URL, the URL path is malformed
    /// or relative, or the decoded path is no longer an existing directory.
    /// The caller then keeps the PTY default (`$HOME`/`USERPROFILE`, else the
    /// process cwd; ghostty `working-directory = home` parity).
    fn inherited_cwd_for(&self, target: ViewId) -> Option<PathBuf> {
        if let Some(source) = self.focused_view() {
            if source != target {
                let report = match self.pane_sessions.get(&source) {
                    Some(session) => session.state.cwd_report(),
                    // A session-less focused leaf (the primary grid owner) reports
                    // through the runtime-global state.
                    None => self.state.cwd_report(),
                };
                if let Some(report) = report {
                    if let Some(path) = osc7_cwd_path(report) {
                        if path.is_dir() {
                            return Some(path);
                        }
                    }
                }
            }
        }
        // CTX-0393: a restored session seeds the spawn cwd from the captured
        // `OSC 7` report when no live pane has reported a usable directory
        // yet. Still fail-open: `None` keeps the PTY default.
        self.session_pending_cwd(&target)
    }

    /// Spawns `program` with `args` as the private shell of layout leaf
    /// `view`, sized to `cols` x `rows` cells.
    ///
    /// Direct argv exec, no shell interpolation: the identical sandbox to
    /// [`spawn_shell_with_args`](Self::spawn_shell_with_args) — `program`
    /// plus `args` pass verbatim to the platform exec path, and a blank
    /// `program` is rejected before any spawn is attempted.
    ///
    /// The leaf must exist in the current layout; spawning for an unknown id
    /// is rejected. Re-spawning a leaf that already owns a session replaces
    /// it: the old `Pty` drops, killing and reaping its child without
    /// leaking a zombie (same lifecycle as
    /// [`spawn_shell_with_args`](Self::spawn_shell_with_args)). The session
    /// starts with a fresh grid resized to `cols` x `rows`
    /// ([`State::resize`] clamps to `1..=1000` per dimension; zero dims
    /// clamp to 1).
    ///
    /// The pane reader starts direct and is promoted into a wakeup
    /// forwarder when a waker is installed (CTX-0230):
    /// [`poll_pty`](Self::poll_pty) drains every pane session on each call,
    /// and a promoted pane additionally wakes the event loop per batch (CTX-0476
    /// waker merge) so pane-only output (e.g. a fresh split shell's `ESC[c`) is answered
    /// promptly. Pane replies flush to the pane's own writer on the same path.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when `program` is blank or `view` is
    /// not a leaf of the current layout; [`RuntimeError::Pty`] when the
    /// platform reports spawn failure.
    pub fn spawn_shell_for_view(
        &mut self,
        view: ViewId,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        self.spawn_shell_for_view_in(view, program, args, cols, rows, None)
    }

    /// [`Self::spawn_shell_for_view`] with an explicit working directory.
    ///
    /// `cwd` of `Some` replaces the CTX-0357 `OSC 7` inheritance for this
    /// spawn (#1528: `bitty ctl terminal spawn --cwd`). It is re-checked here,
    /// at the spawn boundary, so a directory removed after the caller
    /// validated it fails closed instead of silently falling back to another
    /// directory. `None` keeps the inheritance unchanged.
    ///
    /// # Errors
    ///
    /// As [`Self::spawn_shell_for_view`], plus
    /// [`RuntimeError::InvalidConfig`] when `cwd` is not an existing directory.
    pub fn spawn_shell_for_view_in(
        &mut self,
        view: ViewId,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
    ) -> Result<(), RuntimeError> {
        self.spawn_shell_for_view_impl(view, program, args, cols, rows, cwd, &[])
    }

    /// [`Self::spawn_shell_for_view`] with inherited-environment removals
    /// (minimized editor env, W-103 G-2).
    ///
    /// Every entry of `scrub_env` is removed from the inherited session
    /// environment before the child starts; the builder defaults
    /// (`TERM`/`COLORTERM`/`TERM_PROGRAM`) and any ambient key outside the
    /// removal list still apply. The hosted editor leaf passes the
    /// minimized-env removals here so the allowlisted editor starts without
    /// ambient credentials.
    ///
    /// # Errors
    ///
    /// As [`Self::spawn_shell_for_view`].
    pub fn spawn_shell_for_view_scrubbed(
        &mut self,
        view: ViewId,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        scrub_env: &[std::ffi::OsString],
    ) -> Result<(), RuntimeError> {
        self.spawn_shell_for_view_impl(view, program, args, cols, rows, None, scrub_env)
    }

    // Leaf-spawn arity is inherent to the spawn seam (view + program + argv
    // + geometry + cwd + env removals); the two public callers keep their
    // stable arities and share this body so scrubbed and plain spawns cannot
    // drift apart.
    #[allow(clippy::too_many_arguments)]
    fn spawn_shell_for_view_impl(
        &mut self,
        view: ViewId,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
        scrub_env: &[std::ffi::OsString],
    ) -> Result<(), RuntimeError> {
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        if cwd.is_some_and(|dir| !dir.is_dir()) {
            return Err(RuntimeError::InvalidConfig(
                "spawn cwd is not an existing directory",
            ));
        }
        if !self.layout.leaf_ids().contains(&view) {
            return Err(RuntimeError::InvalidConfig(
                "view is not a leaf of the current layout",
            ));
        }
        // CTX-0343: a pane bind turns (or keeps) the leaf at `terminal`
        // content; a previously inert `ws:`/`view:` selector that first
        // matches now is checked before the session is committed. Fail closed
        // before any spawn.
        let label = self.active_workspace_label();
        self.validate_view_target_at("terminal", label, view)?;
        // CTX-1080: declarative panel rules evaluated deterministically at
        // spawn from already-available metadata only (program plus argv,
        // empty title, terminal content). First match wins; empty rules mean
        // no override. Invalid patterns never match (fail-closed skip).
        let matched_rule = {
            let cmd_line = bitty_config::panel_rules::command_line_for(program, args);
            bitty_config::panel_rules::find_match(
                &self.panel_spawn_rules,
                &cmd_line,
                "",
                "terminal",
            )
            .map(|(_, rule)| rule.clone())
        };
        let mut cols = cols.max(1);
        let mut rows = rows.max(1);
        if let Some(rule) = matched_rule.as_ref() {
            if let Some(w) = rule.width {
                cols = w.max(1);
            }
            if let Some(h) = rule.height {
                rows = h.max(1);
            }
        }
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        // W-103 G-2: caller-requested inherited-environment removals
        // (minimized editor env). Removals precede the explicit overrides,
        // so the builder defaults still win for the same key.
        for key in scrub_env {
            builder = builder.env_remove(key.clone());
        }
        // CTX-0357: new panes inherit the focused pane's last `OSC 7` cwd
        // when it still names an existing directory; otherwise the builder
        // keeps the platform default (fail-open, never an error here). An
        // explicit `cwd` (checked above) takes precedence (#1528).
        if let Some(dir) = cwd {
            builder = builder.cwd(dir);
        } else if let Some(inherited) = self.inherited_cwd_for(view) {
            builder = builder.cwd(inherited);
        }
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // All fallible steps done: publish the session. A replaced session's
        // old `Pty` drops here, killing + reaping its child (no zombie).
        // Its forwarder is joined with a bound (CTX-0472) instead of
        // detached: take the old session out first so its receiver/`Pty`
        // drop (EOF unblocks the pump, `send` fails fast), join the old
        // handle, then publish the fresh session.
        // CTX-0297: the pane terminal captures the configured scrollback
        // capacity at creation (restart-required reload class).
        let mut state = State::with_scrollback_lines(self.config.scrollback);
        // CTX-0756 (issue #1359): every spawned pane seeds the configured
        // default cursor shape (effective `terminal.cursor_style`), like the
        // primary terminal at construction.
        state.set_default_cursor_style(self.config.cursor_style);
        state.resize(cols as usize, rows as usize);
        let old_session = self.pane_sessions.remove(&view);
        let old_handle = old_session.and_then(|mut old| {
            drop(old.forward_rx.take());
            old.forward_handle.take()
            // `old` (including its `Pty`) drops here: EOF unblocks pump.
        });
        if let Some(handle) = old_handle {
            let _ = super::join_forwarder_with_timeout(handle, super::FORWARDER_JOIN_TIMEOUT);
        }
        self.pane_sessions.insert(
            view,
            PaneSession {
                parser: Parser::new(),
                state,
                query_overlap: Vec::new(),
                pty,
                reader: Some(reader),
                forward_rx: None,
                forward_handle: None,
                writer,
                last_presented_generation: u64::MAX,
            },
        );
        // CTX-0254 (PX-1588): a respawned leaf starts with a fresh grid, so
        // drop the previous session's placements with it. Without this, a
        // replace on the same `ViewId` inherits the dead grid's placements
        // onto the fresh grid (same stale-pixel class the close path fixes).
        // Stored images survive inertly under the store caps.
        self.kitty_images.clear_origin(Some(view.0));
        // Issue #1762: a respawn reuses the same `ViewId`, so drop its
        // pointer-shape stack with the old session. Without this the fresh
        // shell inherits the dead session's cursor (e.g. stuck `wait`) until
        // it emits its own `OSC 22` (same staleness class as the
        // `kitty_images` cleanup above and the close-path removal).
        self.pointer_stacks.remove(&view);
        // CTX-1011 (#1775): a (re)spawn retires the previous session on this
        // leaf, so its buffered Kitty `OSC 99` groups die with it. The fresh
        // session reusing the same `ViewId` starts empty (session epoch
        // advances) and can never complete stale text.
        self.kitty_assembler
            .discard_stream(super::bell::KittyStreamId::Pane(view.0));
        // CTX-0393: a restored session hydrates the fresh grid with the
        // captured scrollback (immutable history; the shell itself is new).
        // No-op without a pending restore for this leaf.
        let _ = self.hydrate_session_pending_for(view);
        // CTX-0585: a split spawned for the restored primary owner consumes
        // the captured cwd exactly once. `inherited_cwd_for` read it above;
        // clear only after the successful publish so a failed spawn keeps it.
        if self.session_primary_cwd.as_ref().map(|(owner, _)| *owner) == Some(view) {
            self.session_primary_cwd = None;
        }
        // If a waker is already installed (split after `set_pty_waker`),
        // promote immediately so the new pane wakes the loop too (CTX-0230).
        if self.pty_waker.is_some() {
            self.promote_pane_reader_to_forwarder(view);
        }
        // CTX-0532: a fresh session starts with default modes; when it is the
        // focused leaf, the cached/reader state must read its register, not
        // a previous pane's. No-op when another pane is focused.
        self.sync_mode_caches_to_focus();
        // CTX-0803 (#1476): a respawn replaced this leaf's grid, so a
        // selection owned by it addresses cells of a grid that no longer
        // exists. Dropping it is the only honest option (the new grid has no
        // equivalent range).
        self.drop_view_bindings_for(view);
        // CTX-1080: apply the matched rule deterministically after a
        // successful spawn. Presentation stamps the leaf verbatim (the solver
        // ignores it, so solver output stays byte-identical). Dims are a
        // spawn-time initial-size request only: they size the PTY and session
        // grid here and resize the leaf stored size to match, but steady-state
        // geometry is solver-owned (`present_frames` derives cols/rows from
        // the decorated content frames; `sync_pane_geometry_to` and
        // `reflow_present_layout` reflow grid, PTY, and leaf back to the
        // solver frame). Workspace moves only when the spawned
        // leaf is focused and the target exists, otherwise skipped
        // fail-closed. Centered is placement intent for floating panels
        // (floats already center via the overlay frame), so no extra step.
        if let Some(rule) = matched_rule.as_ref() {
            if let Some(presentation) = rule.presentation {
                let mode = match presentation {
                    bitty_config::panel_rules::PanelPresentation::Tiled => {
                        bitty_ui::presentation::PresentationMode::Tiled
                    }
                    bitty_config::panel_rules::PanelPresentation::Floating => {
                        bitty_ui::presentation::PresentationMode::Floating
                    }
                    bitty_config::panel_rules::PanelPresentation::Scratchpad => {
                        bitty_ui::presentation::PresentationMode::Scratchpad
                    }
                };
                if let Some(leaf) = self.layout.find_leaf_mut(view) {
                    leaf.set_presentation(mode);
                }
                for slot in &mut self.workspaces {
                    if let Some(leaf) = slot.layout.find_leaf_mut(view) {
                        leaf.set_presentation(mode);
                    }
                }
                if rule.width.is_some() || rule.height.is_some() {
                    let w = rule.width.unwrap_or(cols).max(1) as usize;
                    let h = rule.height.unwrap_or(rows).max(1) as usize;
                    if let Some(leaf) = self.layout.find_leaf_mut(view) {
                        leaf.resize(w, h);
                    }
                }
            } else if rule.width.is_some() || rule.height.is_some() {
                let w = rule.width.unwrap_or(cols).max(1) as usize;
                let h = rule.height.unwrap_or(rows).max(1) as usize;
                if let Some(leaf) = self.layout.find_leaf_mut(view) {
                    leaf.resize(w, h);
                }
            }
            if let Some(target) = rule.workspace {
                let target_idx = (target as usize).saturating_sub(1);
                if self.focused_view() == Some(view)
                    && target_idx < self.workspaces.len()
                    && target_idx != self.active_workspace_index()
                {
                    let _ = self.workspace_move_focused_to(target_idx);
                }
            }
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Moves one pane's direct [`PtyReader`] into a wakeup-forwarder thread
    /// when a waker is installed (CTX-0230). Mirrors
    /// [`promote_pty_reader_to_forwarder`](Self::promote_pty_reader_to_forwarder):
    /// the forwarder blocks in `recv` (zero wakeups when quiet), forwards
    /// each batch into a bounded channel
    /// ([`PTY_FORWARD_CAPACITY_CHUNKS`]), and invokes the shared waker once
    /// per batch plus once on EOF (CTX-0476 waker merge). [`poll_pty`](Self::poll_pty)
    /// drains the forwarding channel, so the bounded-drain contract holds
    /// end to end. Idempotent: no-op without a session, without a waker, or
    /// when already promoted.
    pub(super) fn promote_pane_reader_to_forwarder(&mut self, view: ViewId) {
        if self.pty_waker.is_none() {
            return;
        }
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            return;
        };
        if sess.forward_rx.is_some() || sess.reader.is_none() {
            return;
        }
        let Some(reader) = sess.reader.take() else {
            return;
        };
        let Some(waker) = self.pty_waker.clone() else {
            sess.reader = Some(reader);
            return;
        };
        match super::pty::spawn_forwarder_default(reader, waker) {
            Ok(parts) => {
                sess.forward_rx = Some(parts.rx);
                sess.forward_handle = Some(parts.handle);
            }
            Err(failure) => {
                // Fail-closed (CTX-0473): the pane keeps its direct pump.
                if let Some(reader) = failure.reader {
                    sess.reader = Some(reader);
                }
                self.forwarder_spawn_failures = self.forwarder_spawn_failures.wrapping_add(1);
                if let Some(suppressed) = self.spawn_log.admit_now() {
                    eprintln!(
                        "bitty: pane wakeup forwarder spawn failed for {view:?} ({}): using direct pump{}",
                        failure.error,
                        log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
    }

    /// Whether one pane's reader is promoted into a wakeup forwarder
    /// (implies a pane session exists). Introspection parity with
    /// [`has_pty_forwarder`](Self::has_pty_forwarder).
    #[must_use]
    pub fn has_pane_forwarder(&self, view: &ViewId) -> bool {
        self.pane_sessions
            .get(view)
            .is_some_and(|sess| sess.forward_rx.is_some())
    }

    /// Whether leaf `view` owns a private shell session.
    #[must_use]
    pub fn has_pane_session(&self, view: &ViewId) -> bool {
        self.pane_sessions.contains_key(view)
    }

    /// Number of live per-pane shell sessions.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.pane_sessions.len()
    }

    /// Ids owning a private shell session, in deterministic (`ViewId`) order.
    #[must_use]
    pub fn pane_session_ids(&self) -> Vec<ViewId> {
        self.pane_sessions.keys().copied().collect()
    }

    /// How many split leaves own a private shell session (issue #1356).
    ///
    /// The embedder keeps the window open while split sessions exist even
    /// after the primary shell exits; a lone primary exit closes the
    /// session (ghostty/kitty close the window on child exit).
    #[must_use]
    pub fn pane_session_count(&self) -> usize {
        self.pane_sessions.len()
    }

    /// Process id of the leaf's shell child, when the session exists and the
    /// platform reports one.
    #[must_use]
    pub fn pane_pid(&self, view: &ViewId) -> Option<u32> {
        self.pane_sessions.get(view).and_then(|sess| sess.pty.pid())
    }

    /// Signals the leaf child's recorded owned process tree (W-103 G-3).
    ///
    /// Every PTY child travels with the tree assembled at its spawn (see
    /// [`bitty_pty::Pty::tree`]): on Unix it heads its own process group,
    /// on Windows it is born inside its Job Object, so the tree covers the
    /// whole tree the child grew (editors spawning shells, pagers,
    /// background jobs) with no adopt-after-start window (CTX-0978). The
    /// kill signal reaches every member, not just the direct child that
    /// [`close_pane_session`](Self::close_pane_session) would reap alone.
    /// The tracker is dropped without retiring: the `PaneSession` still owns
    /// the leader and reaps it on teardown, which is the only place a reap
    /// may happen. Returns `true` when the tree signal was delivered;
    /// `false` without a session, without a stored tree, on platforms
    /// without an owned-tree backend, or when the tree is already gone
    /// (callers still run the normal teardown, which is always safe).
    pub fn kill_pane_tree(&self, view: &ViewId) -> bool {
        let Some(sess) = self.pane_sessions.get(view) else {
            return false;
        };
        let Some(tree) = sess.pty.tree() else {
            return false;
        };
        tree.signal(bitty_pty::TreeSignal::Kill).is_ok()
    }

    /// Exit status of the leaf's child when it has already exited, without
    /// blocking (CTX-0731, #982).
    ///
    /// Mirrors [`pane_pid`](Self::pane_pid): `None` when the leaf owns no
    /// session, the child is still running, or the platform reports no
    /// status (including an already-reaped child). The composer
    /// external-editor host polls this once per event-loop tick to detect
    /// editor exit and round the edited buffer back.
    pub fn pane_try_wait(&mut self, view: &ViewId) -> Option<bitty_pty::ExitStatus> {
        let sess = self.pane_sessions.get_mut(view)?;
        sess.pty.try_wait().ok()?
    }

    /// Current kernel winsize of the leaf's PTY, when the session exists.
    ///
    /// Introspection for split/resize verification (CTX-0269):
    /// [`sync_pane_geometry`](Self::sync_pane_geometry) resizes the winsize
    /// alongside the grid, so this must track the leaf allocation after any
    /// layout change. Best-effort (`None` without a session or when the
    /// kernel query fails).
    #[must_use]
    pub fn pane_pty_size(&self, view: &ViewId) -> Option<(u16, u16)> {
        self.pane_sessions
            .get(view)
            .and_then(|sess| sess.pty.size().ok())
    }

    /// Read-only snapshot of the leaf's private grid, when it owns a
    /// session. Leaves without a session share the primary
    /// [`snapshot`](Self::snapshot) only while they are the primary owner
    /// (CTX-0359).
    #[must_use]
    pub fn pane_snapshot(&self, view: &ViewId) -> Option<Snapshot> {
        self.pane_sessions
            .get(view)
            .map(|sess| sess.state.snapshot())
    }

    /// Tears down the leaf's private shell session, if any. The owned `Pty`
    /// drops, killing and reaping the child without leaking a zombie. Its
    /// forwarder is joined with a bound (CTX-0472) instead of detached.
    /// Returns true when a session was removed.
    pub fn close_pane_session(&mut self, view: &ViewId) -> bool {
        // CTX-0370: a pending close confirmation for this pane dies with its
        // session, so a later unrelated close can never "confirm" a stale arm.
        self.clear_pending_close_for_view(*view);
        let old_session = self.pane_sessions.remove(view);
        let Some(mut old) = old_session else {
            return false;
        };
        // Drop receiver + `Pty` first (EOF unblocks pump, `send` fails
        // fast), then bound the join. Timeout detaches but never hangs
        // the close path on a wedged child.
        drop(old.forward_rx.take());
        let old_handle = old.forward_handle.take();
        drop(old);
        if let Some(handle) = old_handle {
            let _ = super::join_forwarder_with_timeout(handle, super::FORWARDER_JOIN_TIMEOUT);
        }
        // CTX-0254: drop the closed pane's placements with its grid, so
        // a later leaf reusing the numeric id can never inherit stale
        // image pixels (origin tokens are `ViewId.0` values).
        self.kitty_images.clear_origin(Some(view.0));
        // Issue #1762: a pane exit resets its pointer shape. Dropping the
        // closed leaf's stack means a later leaf reusing the numeric id
        // starts from the default pointer, and the focused icon falls back
        // to the new focus (or default) on the next tick sync.
        self.pointer_stacks.remove(view);
        // CTX-1011 (#1775): the closed session's buffered Kitty `OSC 99`
        // groups die with it, so a later leaf reusing the numeric id can
        // never complete stale notification text.
        self.kitty_assembler
            .discard_stream(super::bell::KittyStreamId::Pane(view.0));
        // CTX-0532: the focused pane's session may have just vanished (or a
        // hidden one closed); re-attribute the mode caches. The reader paths
        // consult `focused_modes` directly and fall back to the primary
        // register for a now-session-less focused leaf.
        self.sync_mode_caches_to_focus();
        // CTX-0803 (#1476): the grid a selection owned by this leaf addressed
        // is gone. Drop it rather than let it fall back to the primary grid.
        self.drop_view_bindings_for(*view);
        self.pending_full_redraw = true;
        true
    }

    /// Re-syncs every pane session's grid + PTY winsize to its leaf's
    /// current allocation (CTX-0176). Called after layout reflows that can
    /// move leaf boundaries (`set_layout`, `reflow_to_grid`) so split panes
    /// track window resizes, DPI reflows, and split-ratio changes exactly
    /// like the primary session. Per-pane best-effort: leaves whose dims
    /// already match are skipped (keeps generations stable for
    /// frame-on-demand); a PTY resize error never fails the reflow.
    pub(super) fn sync_pane_geometry(&mut self) {
        let frames = self.present_frames();
        self.sync_pane_geometry_to(&frames);
    }

    /// Frame-taking core of [`Self::sync_pane_geometry`] (CTX-0405).
    ///
    /// The present path already holds this frame's decorated content frames;
    /// passing them in keeps the per-frame sync from recomputing them. The
    /// resize rule is identical: only sessions whose grid or PTY winsize
    /// differs from their leaf frame are touched.
    pub(super) fn sync_pane_geometry_to(&mut self, frames: &[PresentFrame]) {
        if self.pane_sessions.is_empty() {
            return;
        }
        // CTX-0294: decorated content frames (Core px decoration + CTX-0177
        // cell gaps) so pane grids/PTYs match the painted viewport.
        for frame in frames {
            let cols = frame.cols.max(1);
            let rows = frame.rows.max(1);
            if let Some(sess) = self.pane_sessions.get_mut(&frame.view) {
                if sess.state.width() != cols as usize || sess.state.height() != rows as usize {
                    // CTX-0312: a single resize preserves visible content;
                    // trailing blank viewport rows absorb the height shrink
                    // before any bottom-align into scrollback, so the former
                    // two-phase workaround is gone.
                    let _ = sess.state.resize(cols as usize, rows as usize);
                    let _ = sess.pty.resize(cols, rows);
                }
            }
        }
    }

    /// Feeds raw PTY bytes from one pane's shell into that pane's private
    /// grid through the exact [`handle_pty_bytes`](Self::handle_pty_bytes)
    /// pipeline (query replies, clipboard policy, cold bridge, search).
    ///
    /// Implemented by swapping the pane's `Parser`/`State`/overlap tail into
    /// the primary slots for the call and swapping back afterwards, so panes
    /// get full pipeline fidelity with no duplicated logic. Unknown ids and
    /// empty input are no-ops. Single-threaded (`&mut self`), and nothing in
    /// the [`handle_pty_bytes`](Self::handle_pty_bytes) call tree touches
    /// `pane_sessions`, so the session cannot vanish mid-call; that call's
    /// documented never-panics-over-untrusted-bytes contract keeps the swap
    /// pair total.
    pub fn handle_pane_bytes(&mut self, view: ViewId, bytes: &[u8]) {
        if bytes.is_empty() || !self.pane_sessions.contains_key(&view) {
            return;
        }
        // CTX-0254: tag Kitty placements emitted by this drain with the
        // pane's origin token, so the present layer confines them to this
        // pane's leaf (a background pane can never paint over the focused
        // pane). Saved and restored around the shared pipeline like the
        // parser/state swap pair below.
        let prev_origin = self.kitty_origin;
        self.kitty_origin = Some(view.0);
        {
            let Some(sess) = self.pane_sessions.get_mut(&view) else {
                self.kitty_origin = prev_origin;
                return;
            };
            std::mem::swap(&mut self.parser, &mut sess.parser);
            std::mem::swap(&mut self.state, &mut sess.state);
            std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
        }
        // CTX-0532: the inner parse call owns no cache sync, so the swapped
        // grid can never be cached; the outer sync below runs after the swap
        // back with the pane's register authoritative again.
        self.handle_pty_bytes_inner(bytes);
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            // Unreachable single-threaded (see doc above); keep total rather
            // than debug-panicking on a corrupted swap pair.
            self.kitty_origin = prev_origin;
            debug_assert!(false, "pane session vanished mid-pump");
            return;
        };
        std::mem::swap(&mut self.parser, &mut sess.parser);
        std::mem::swap(&mut self.state, &mut sess.state);
        std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
        self.kitty_origin = prev_origin;
        // CTX-0532: mode changes landed on the pane's register; re-attribute
        // the input-mode caches to the focused pane.
        self.sync_mode_caches_to_focus();
    }

    /// View whose grid the output pipeline is feeding right now (CTX-0805).
    ///
    /// [`Self::handle_pane_bytes`] swaps the pane's grid into the primary
    /// slot and tags the drain with that pane's origin token, so inside the
    /// shared `handle_pty_bytes_inner` pipeline `self.state` is the pane's
    /// grid. Outside a pane drain the pipeline feeds the primary grid, owned
    /// by `primary_view`. View-bound consumers (selection, search) use this
    /// to react only to output on the grid they address.
    pub(super) fn fed_grid_view(&self) -> Option<ViewId> {
        match self.kitty_origin {
            Some(raw) => Some(ViewId::new(raw)),
            // Same precedence as `selection::grid_of`: a primary owner that
            // also holds a pane session (re-homed onto a split leaf) reads
            // that session's grid, so the primary drain feeds a grid no View
            // resolves to and must not name the owner.
            None => self
                .primary_view
                .filter(|view| !self.pane_sessions.contains_key(view)),
        }
    }

    /// Flushes one pane's queued terminal replies (DA/DECRQM/XTGETTCAP,
    /// OSC 52 read answers) to that pane's own PTY master.
    ///
    /// Per-pane mirror of [`write_replies`](Self::write_replies): bounded
    /// (4 KiB reply cap, fail-closed), no-op without a session or when
    /// empty. Returns bytes written. [`poll_pty`](Self::poll_pty)
    /// already calls this per drained pane; embedders also call it after
    /// `tick` (like the primary post-tick flush) so replies queued outside
    /// the pump still reach the pane's shell promptly.
    pub fn write_pane_replies(&mut self, view: ViewId) -> usize {
        use std::io::Write as _;
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            return 0;
        };
        let replies = sess.state.take_replies();
        if replies.is_empty() {
            return 0;
        }
        // Each chunk is bounded; total bounded by the reply cap (4 KiB).
        // Best-effort, fail-closed (CTX-0473): the lost remainder is accounted,
        // never silently swallowed.
        let (total, dropped) = super::pty::write_chunks(&mut sess.writer, &replies);
        let flush_failed = sess.writer.flush().is_err();
        if dropped > 0 {
            self.reply_write_dropped_bytes =
                self.reply_write_dropped_bytes.wrapping_add(dropped as u64);
        }
        if flush_failed {
            self.write_flush_failures = self.write_flush_failures.wrapping_add(1);
        }
        total
    }

    /// Mode register of the input-owning context: the focused leaf's private
    /// session modes when it owns one, otherwise the primary grid's modes.
    ///
    /// CTX-0532: every mode-sensitive input reader (mouse capture, Kitty
    /// encoding, focus reporting, bracketed paste) must attribute to the
    /// *focused* pane. Reading `self.state` directly attributed them to the
    /// primary grid even while another pane owned the keyboard; a focus
    /// transition with no intervening PTY pump then used the previous pane's
    /// modes. Session-less leaves keep the documented primary fallback,
    /// matching [`Self::sync_mode_caches_to_focus`].
    pub(super) fn focused_modes(&self) -> &bitty_term_state::Modes {
        let focused = self.focus.focused();
        match focused.and_then(|id| self.pane_sessions.get(&id)) {
            Some(sess) => sess.state.modes(),
            None => self.state.modes(),
        }
    }

    /// Whether leaf `view`'s app tracks the mouse, with the same attribution
    /// rule as [`Self::focused_modes`] (CTX-0804): the leaf's own session
    /// modes, else the primary grid's modes when `view` is the primary owner.
    /// A session-less, non-primary leaf runs no app, so it tracks nothing.
    pub(super) fn view_tracks_mouse(&self, view: ViewId) -> bool {
        self.session_state_for(view)
            .is_some_and(|state| state.modes().mouse_tracking.is_some())
    }

    /// Whether the focused pane's grid is on the alternate screen.
    ///
    /// Mirrors [`Self::focused_modes`] attribution (CTX-0532): alternate
    /// scroll (`?1007`) must read the focused pane's own screen, with the
    /// primary grid as the session-less fallback. Used by the wheel path to
    /// decide between cursor-key translation and viewport scrolling.
    pub(super) fn focused_alt_screen_active(&self) -> bool {
        let focused = self.focus.focused();
        match focused.and_then(|id| self.pane_sessions.get(&id)) {
            Some(sess) => sess.state.alt_screen_active(),
            None => self.state.alt_screen_active(),
        }
    }

    /// Re-syncs the global Kitty/mouse-capture caches to the focused leaf's
    /// grid (or the primary grid when focus owns no session).
    ///
    /// CTX-0532: called on every focus transition and after any path that
    /// mutates a mode register (PTY apply, pane pump, spawn/close), so the
    /// public telemetry caches never lag the focused pane. The mode-sensitive
    /// reader paths read [`Self::focused_modes`] directly; these caches only
    /// mirror them for `enhanced_keyboard_flags()` / `mouse_capture_active()` observers.
    pub(super) fn sync_mode_caches_to_focus(&mut self) {
        let (enhanced_flags, mouse) = {
            let modes = self.focused_modes();
            (
                modes.enhanced_keyboard.flags(),
                modes.mouse_tracking.is_some(),
            )
        };
        self.enhanced_keyboard_flags = enhanced_flags;
        self.mouse_capture_enabled = mouse;
    }
}

/// Extracts the local path from an `OSC 7` cwd report (CTX-0357).
///
/// Only `file://` URLs are trusted — the sole scheme shell integration emits
/// for a local cwd; any other scheme fails open to the platform default. The
/// authority component is ignored (ghostty parity), the path is
/// percent-decoded, and the result must be absolute for the target platform.
/// Returns `None` for a missing/relative path, malformed escapes, or
/// non-UTF-8 decoded bytes.
///
/// Shared with the session-restore spawn-cwd fallback (CTX-0393), which
/// replays a captured report through the same validation.
pub(super) fn osc7_cwd_path(report: &str) -> Option<PathBuf> {
    let rest = report.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let decoded = String::from_utf8(percent_decode(&rest[slash..])?).ok()?;
    #[cfg(windows)]
    let decoded = strip_windows_drive_root(decoded);
    let path = PathBuf::from(decoded);
    path.is_absolute().then_some(path)
}

/// Windows `file://` URLs encode the drive root as `/C:/...`; drop the
/// leading slash so `Path::is_absolute` sees the drive prefix.
#[cfg(windows)]
fn strip_windows_drive_root(path: String) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_string()
    } else {
        path
    }
}

/// Decodes `%XX` escapes to bytes; `None` on a truncated escape or a
/// non-hex digit. `+` stays literal (URI path semantics, not form data).
fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_digit(*bytes.get(i + 1)?)?;
            let lo = hex_digit(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Hex digit value for `%XX` decoding.
const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn osc7_path_accepts_plain_and_authority_file_urls() {
        assert_eq!(
            osc7_cwd_path("file:///home/user"),
            Some(PathBuf::from("/home/user"))
        );
        assert_eq!(
            osc7_cwd_path("file://localhost/home/user"),
            Some(PathBuf::from("/home/user"))
        );
        assert_eq!(
            osc7_cwd_path("file://some-host/home/user"),
            Some(PathBuf::from("/home/user"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn osc7_path_accepts_windows_drive_file_urls() {
        assert_eq!(
            osc7_cwd_path("file:///C:/Users/me"),
            Some(PathBuf::from("C:/Users/me"))
        );
        assert_eq!(
            osc7_cwd_path("file://host/C:/Users/me"),
            Some(PathBuf::from("C:/Users/me"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn osc7_path_percent_decodes() {
        assert_eq!(
            osc7_cwd_path("file:///home/user/my%20dir"),
            Some(PathBuf::from("/home/user/my dir"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn osc7_path_percent_decodes_windows() {
        assert_eq!(
            osc7_cwd_path("file:///C:/Users/my%20dir"),
            Some(PathBuf::from("C:/Users/my dir"))
        );
    }

    #[test]
    fn osc7_path_rejects_untrusted_or_malformed_forms() {
        assert_eq!(osc7_cwd_path("kitty-shell-cwd://host/home/user"), None);
        assert_eq!(osc7_cwd_path("https://example.com/home/user"), None);
        assert_eq!(osc7_cwd_path("file:///home/user%2"), None);
        assert_eq!(osc7_cwd_path("file:///home/user%zz"), None);
        // No path component at all (authority-only URL): rejected.
        assert_eq!(osc7_cwd_path("file://host"), None);
        assert_eq!(osc7_cwd_path("file:///%FF%FE"), None);
        assert_eq!(osc7_cwd_path(""), None);
    }

    #[test]
    fn percent_decode_keeps_plus_literal() {
        assert_eq!(percent_decode("/a+b").as_deref(), Some(&b"/a+b"[..]));
    }
}
