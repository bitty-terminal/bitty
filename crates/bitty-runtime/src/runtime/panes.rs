//! `Runtime` — Per-leaf pane sessions (spawn, close, pump, sync).
//!
//! Split from `super` (`runtime.rs`) as a pure move under CTX-0232:
//! byte-identical logic, only module wiring changed.
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
        let cols = self.cols.min(u16::MAX as usize) as u16;
        let rows = self.rows.min(u16::MAX as usize) as u16;
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // Replace any previously spawned child (drop kills old). A prior
        // wakeup forwarder (if any) is detached: it owns the old reader and
        // exits on EOF/disconnect once the old PTY is dropped.
        self.pty_forward_rx = None;
        self.pty_forward_handle = None;
        self.pty = Some(pty);
        self.pty_reader = Some(reader);
        self.pty_writer = Some(writer);
        // If a waker is already installed (respawn after `set_pty_waker`),
        // promote immediately so the new child wakes the loop too.
        if self.pty_waker.is_some() {
            self.promote_pty_reader_to_forwarder();
        }
        // Clear pending input on new shell: fresh session, no stale keystrokes.
        self.pending_input.clear();
        self.pending_input_dropped = 0;
        Ok(())
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
    /// and a promoted pane additionally wakes the event loop per chunk so
    /// pane-only output (e.g. a fresh split shell's `ESC[c`) is answered
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
        if program.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("program must not be empty"));
        }
        if !self.layout.leaf_ids().contains(&view) {
            return Err(RuntimeError::InvalidConfig(
                "view is not a leaf of the current layout",
            ));
        }
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut builder = PtyBuilder::new(program).size(cols, rows);
        for arg in args {
            builder = builder.arg(*arg);
        }
        let mut pty = builder.spawn().map_err(RuntimeError::from)?;
        let reader = pty.take_reader().map_err(RuntimeError::from)?;
        let writer = pty.take_writer().map_err(RuntimeError::from)?;
        // All fallible steps done: publish the session. A replaced session's
        // old `Pty` drops here, killing + reaping its child (no zombie).
        // CTX-0297: the pane terminal captures the configured scrollback
        // capacity at creation (restart-required reload class).
        let mut state = State::with_scrollback_lines(self.config.scrollback);
        state.resize(cols as usize, rows as usize);
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
            },
        );
        // CTX-0254 (PX-1588): a respawned leaf starts with a fresh grid, so
        // drop the previous session's placements with it. Without this, a
        // replace on the same `ViewId` inherits the dead grid's placements
        // onto the fresh grid (same stale-pixel class the close path fixes).
        // Stored images survive inertly under the store caps.
        self.kitty_images.clear_origin(Some(view.0));
        // If a waker is already installed (split after `set_pty_waker`),
        // promote immediately so the new pane wakes the loop too (CTX-0230).
        if self.pty_waker.is_some() {
            self.promote_pane_reader_to_forwarder(view);
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Moves one pane's direct [`PtyReader`] into a wakeup-forwarder thread
    /// when a waker is installed (CTX-0230). Mirrors
    /// [`promote_pty_reader_to_forwarder`](Self::promote_pty_reader_to_forwarder):
    /// the forwarder blocks in `recv` (zero wakeups when quiet), forwards
    /// each chunk into a bounded channel
    /// ([`PTY_FORWARD_CAPACITY_CHUNKS`]), and invokes the shared waker once
    /// per chunk plus once on EOF. [`pump_pane_sessions`](Self::pump_pane_sessions)
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
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(PTY_FORWARD_CAPACITY_CHUNKS);
        let handle = std::thread::Builder::new()
            .name("bitty-pty-wakeup".to_owned())
            .spawn(move || super::pty::pty_forward_loop(reader, tx, waker))
            .expect("std thread spawn cannot fail with default builder options");
        sess.forward_rx = Some(rx);
        sess.forward_handle = Some(handle);
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

    /// Process id of the leaf's shell child, when the session exists and the
    /// platform reports one.
    #[must_use]
    pub fn pane_pid(&self, view: &ViewId) -> Option<u32> {
        self.pane_sessions.get(view).and_then(|sess| sess.pty.pid())
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
    /// [`snapshot`](Self::snapshot).
    #[must_use]
    pub fn pane_snapshot(&self, view: &ViewId) -> Option<Snapshot> {
        self.pane_sessions
            .get(view)
            .map(|sess| sess.state.snapshot())
    }

    /// Tears down the leaf's private shell session, if any. The owned `Pty`
    /// drops, killing and reaping the child without leaking a zombie.
    /// Returns true when a session was removed.
    pub fn close_pane_session(&mut self, view: &ViewId) -> bool {
        let removed = self.pane_sessions.remove(view).is_some();
        if removed {
            // CTX-0254: drop the closed pane's placements with its grid, so
            // a later leaf reusing the numeric id can never inherit stale
            // image pixels (origin tokens are `ViewId.0` values).
            self.kitty_images.clear_origin(Some(view.0));
            self.pending_full_redraw = true;
        }
        removed
    }

    /// Re-syncs every pane session's grid + PTY winsize to its leaf's
    /// current allocation (CTX-0176). Called after layout reflows that can
    /// move leaf boundaries (`set_layout`, `reflow_to_grid`) so split panes
    /// track window resizes, DPI reflows, and split-ratio changes exactly
    /// like the primary session. Per-pane best-effort: leaves whose dims
    /// already match are skipped (keeps generations stable for
    /// frame-on-demand); a PTY resize error never fails the reflow.
    pub(super) fn sync_pane_geometry(&mut self) {
        if self.pane_sessions.is_empty() {
            return;
        }
        // CTX-0177: gapped allocations so pane grids match the shrunk leaves.
        let allocs = self.layout.layout_with_gaps(self.container, self.gaps());
        for (id, rect) in &allocs {
            let cols = rect.width.max(1);
            let rows = rect.height.max(1);
            if let Some(sess) = self.pane_sessions.get_mut(id) {
                if sess.state.width() != cols as usize || sess.state.height() != rows as usize {
                    let _ = sess.state.resize(cols as usize, rows as usize);
                    let _ = sess.pty.resize(cols, rows);
                }
            }
        }
    }

    /// Drains every pane session's reader — the wakeup-forwarder channel
    /// when [`promote_pane_reader_to_forwarder`](Self::promote_pane_reader_to_forwarder)
    /// promoted the pump, otherwise the direct reader — into its private
    /// grid via the shared PTY pipeline (see [`handle_pane_bytes`](Self::handle_pane_bytes)),
    /// flushes pane replies to each pane's own writer, and re-syncs the
    /// global input-mode caches to the focused leaf. Returns the drained
    /// chunk count. No-op when no pane session exists.
    pub(super) fn pump_pane_sessions(&mut self) -> usize {
        if self.pane_sessions.is_empty() {
            return 0;
        }
        // Collect without holding a borrow across the mutable pump calls.
        // `BTreeMap` iteration is `ViewId`-ordered, so multi-pane wakeups
        // are deterministic.
        let mut pending: Vec<(ViewId, Vec<u8>)> = Vec::new();
        for (id, sess) in self.pane_sessions.iter() {
            let mut per_pane = 0usize;
            while per_pane < 1024 {
                // Promoted panes drain the forwarder channel; direct panes
                // drain the pump channel. Either way the bound holds
                // (`CHANNEL_CAPACITY_CHUNKS` x `READ_CHUNK_SIZE` per stage).
                let chunk = if let Some(rx) = sess.forward_rx.as_ref() {
                    rx.try_recv().ok()
                } else if let Some(reader) = sess.reader.as_ref() {
                    reader.try_recv()
                } else {
                    None
                };
                match chunk {
                    Some(chunk) => {
                        debug_assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                        pending.push((*id, chunk));
                        per_pane += 1;
                    }
                    None => break,
                }
            }
        }
        let drained = pending.len();
        for (id, chunk) in pending {
            self.handle_pane_bytes(id, &chunk);
            let _ = self.write_pane_replies(id);
        }
        self.sync_mode_caches_to_focus();
        drained
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
        self.handle_pty_bytes(bytes);
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
    }

    /// Flushes one pane's queued terminal replies (DA/DECRQM/XTGETTCAP,
    /// OSC 52 read answers) to that pane's own PTY master.
    ///
    /// Per-pane mirror of [`write_replies`](Self::write_replies): bounded
    /// (4 KiB reply cap, fail-closed), no-op without a session or when
    /// empty. Returns bytes written. [`pump_pane_sessions`](Self::pump_pane_sessions)
    /// already calls this per drained pane; embedders also call it after
    /// `tick` (like the primary post-tick flush) so replies queued outside
    /// the pump still reach the pane's shell promptly.
    pub fn write_pane_replies(&mut self, view: ViewId) -> usize {
        let Some(sess) = self.pane_sessions.get_mut(&view) else {
            return 0;
        };
        let replies = sess.state.take_replies();
        if replies.is_empty() {
            return 0;
        }
        let mut total = 0usize;
        use std::io::Write as _;
        for chunk in replies {
            // Each chunk is bounded; total bounded by the reply cap (4 KiB).
            // Best-effort, fail-closed: on write error break and drop remainder.
            if sess.writer.write_all(&chunk).is_ok() {
                total += chunk.len();
            } else {
                break;
            }
        }
        let _ = sess.writer.flush();
        total
    }

    /// Re-syncs the global Kitty/mouse-capture caches to the focused leaf's
    /// grid (or the primary grid when focus owns no session). Called after
    /// pumping panes so a mouse-tracking app in the focused pane still takes
    /// effect. No-op equivalent when no pane session exists.
    pub(super) fn sync_mode_caches_to_focus(&mut self) {
        if self.pane_sessions.is_empty() {
            return;
        }
        let focused = self.focus.focused();
        let (kitty, mouse) = match focused.and_then(|id| self.pane_sessions.get(&id)) {
            Some(sess) => {
                let modes = sess.state.modes();
                (modes.kitty_keyboard, modes.mouse_tracking.is_some())
            }
            None => {
                let modes = self.state.modes();
                (modes.kitty_keyboard, modes.mouse_tracking.is_some())
            }
        };
        self.kitty_flags = kitty;
        self.mouse_capture_enabled = mouse;
    }
}
