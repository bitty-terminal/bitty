//! `Runtime` — search/selection host operations (CTX-0936, W-143a–c, #1625).
//!
//! Core-internal host-op surface over the existing search/selection mechanism
//! (`runtime::search`, `bitty-ui::search` + `selection`, `bitty-term-state::search`)
//! under the W-135 contract
//! (`bitty-terminal-docs/specifications/search-selection-contract.md`).
//!
//! What this module publishes (mechanism only):
//! - bounded per-view query and snapshot reads ([`Runtime::search_host_query`],
//!   [`Runtime::search_host_snapshot`]);
//! - View-bound result-set lifecycle with replace-on-refresh coalescing
//!   ([`Runtime::search_host_set_query`], [`Runtime::search_host_refresh`]);
//! - stable line-id identity plus result-generation fencing
//!   ([`SearchResultHandle`], [`SelectionHandle`]);
//! - highlight computation, viewport navigation, and selection driving
//!   ([`Runtime::search_host_highlights`],
//!   [`Runtime::search_host_scroll_to_current`],
//!   [`Runtime::search_host_apply_selection`],
//!   [`Runtime::search_host_install_selection`],
//!   [`Runtime::search_host_persist_live_selection`],
//!   [`Runtime::search_host_restore_selection`]);
//! - the clipboard-permission gate ([`Runtime::search_host_copy_to_clipboard`],
//!   [`Runtime::search_host_yank_selection`]).
//!
//! Typed outcomes: every op returns `Result<_, HostOpError>` with
//! `Denied` (cross-view / missing capability, fail closed, no partial write),
//! `Stale` (superseded generation, pruned or drifted identity), or
//! `Unavailable` (no live grid); never a bare boolean where the reason
//! matters. The legacy boolean seams (`search_apply_selection`, yank paths)
//! are preserved as wrappers that map these outcomes without behavior change.
//!
//! Deliberately out of scope (W-144 / W-01): input UI, result presentation,
//! keymaps, modal policy, plugin Lua code, and SDK bindings. No new
//! capability identifier is introduced: the clipboard gate reuses the existing
//! `clipboard.write` grant. Snapshots are returned to the caller only and are
//! never published on the Event Bus. Safe mode is unaffected: these are Core
//! mechanisms that stay usable with zero plugins. Host ops run off the input,
//! parser, and render hot paths (cold/modal paths only).

use super::selection::grid_of;
use super::*;
use bitty_platform::clipboard::CLIPBOARD_MAX_BYTES;
use bitty_ui::selection::BufferPos;

/// Typed outcome for a search/selection host operation.
///
/// Fail-closed by construction: an error carries the reason and performs no
/// read, no write, and no retarget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOpError {
    /// Cross-view read or missing capability. No data is returned and, for
    /// clipboard writes, nothing is written (no partial write).
    Denied(String),
    /// The handle's result generation was superseded (set/refresh/clear
    /// replaced the set), or its stable line identity no longer resolves
    /// (pruned, reflowed, erased). Never retargets unrelated content.
    Stale(String),
    /// The named View has no live grid in the active layout (closed, hidden,
    /// or never bound). No grid is assumed.
    Unavailable(String),
}

impl std::fmt::Display for HostOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(msg) => write!(f, "search/selection host op denied: {msg}"),
            Self::Stale(msg) => write!(f, "search/selection host op stale: {msg}"),
            Self::Unavailable(msg) => write!(f, "search/selection host op unavailable: {msg}"),
        }
    }
}

impl std::error::Error for HostOpError {}

/// Generation-fenced handle to the View-bound search result set.
///
/// Captures the owning [`ViewId`](bitty_ui::View) plus the runtime's
/// `search_result_generation` at bind/refresh time. Any set, refresh, or
/// clear bumps the generation, so a handle from a prior generation fails
/// closed with [`HostOpError::Stale`] instead of addressing replaced matches.
/// Navigation (`advance`) preserves the generation: moving the current index
/// does not replace the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchResultHandle {
    /// View whose grid the result set was computed against.
    pub view: ViewId,
    /// Result-set generation the handle is valid for.
    pub result_generation: u64,
}

/// Bounded outcome of binding or refreshing a View-bound search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHostQuery {
    /// Fresh handle for the new result-set generation.
    pub handle: SearchResultHandle,
    /// Effective pattern after char-boundary truncation to
    /// `SEARCH_MAX_PATTERN_LEN` (256 bytes).
    pub pattern: String,
    /// Effective options after clamping to `SEARCH_MAX_RESULTS` (1000).
    pub options: SearchOptions,
    /// Number of matches (`<= SEARCH_MAX_RESULTS`), oldest scrollback first.
    pub match_count: usize,
    /// Current navigation index, if any.
    pub current_index: Option<usize>,
    /// Damage generation of the grid the matches were computed against
    /// (informational: tells a caller when a refresh is advisable; the hard
    /// fence is [`SearchResultHandle::result_generation`]).
    pub grid_generation: u64,
}

/// How driving the live selection from the current match resolved.
///
/// Replaces the bare boolean of the legacy `search_apply_selection` seam with
/// a named outcome; the legacy seam maps `Selected` to `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionDriveOutcome {
    /// The live selection now exactly covers the current match.
    Selected,
    /// The match is valid history (readable via its persistent selection)
    /// but not live-selectable; the live selection was cleared.
    HistoryHighlight,
    /// No current match; nothing was touched.
    NoMatch,
}

/// Generation-adjacent handle to a buffer-anchored selection.
///
/// [`PersistentSelection`] already carries stable scrollback line ids, which
/// detect prune drift and reflow reassignment via `is_valid`. This handle adds
/// the owning [`ViewId`](bitty_ui::View) (cross-view restores are denied) and
/// two stamps read at mint time:
///
/// - `epoch`: the grid's `buffer_epoch`, bumped on every wholesale
///   buffer-identity change (scrollback clear, reset, resize/reflow). A
///   mismatched epoch stales the handle before any row is trusted, mirroring
///   the `ZoneRecord` precedent in `bitty-term-state`.
/// - `evicted_at`: the grid's `scrollback_evicted_total()`. Prune shifts
///   combined-buffer rows arithmetically, so the restore subtracts the drift
///   exactly (again like `zone_buffer_row`); only content actually evicted
///   past the window stales.
///
/// No `bitty-ui` shape changes: fencing lives at the host-op layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionHandle {
    /// View whose grid the selection was lifted from and restores into.
    pub view: ViewId,
    /// `State::buffer_epoch()` when minted (wholesale-identity fence).
    pub epoch: u64,
    /// `State::scrollback_evicted_total()` when minted (prune-drift base).
    pub evicted_at: u64,
    /// Buffer-anchored selection with stable line ids.
    pub selection: PersistentSelection,
}

/// Who is asking the clipboard-permission gate for a write.
///
/// `TrustedUser` is the existing Core UX path (keyboard search/copy-mode and
/// mouse gestures): a direct user action, not capability-gated, exactly as
/// today. `Plugin` is a future extension caller: it must hold the existing
/// `clipboard.write` grant for its manifest hash (deny-by-default, no partial
/// write). No new capability identifier is introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardCaller<'a> {
    /// Direct user gesture through Core UX (search/copy-mode, mouse).
    TrustedUser,
    /// Extension caller gated on the existing `clipboard.write` grant.
    Plugin {
        /// Calling plugin.
        plugin_id: &'a PluginId,
        /// Manifest hash the grant is bound to.
        manifest_hash: &'a str,
    },
}

/// Outcome of a gated clipboard write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YankOutcome {
    /// Bytes written (char-boundary truncated to `CLIPBOARD_MAX_BYTES`).
    pub text: String,
    /// Whether the source exceeded the bound and was truncated.
    pub truncated: bool,
}

/// Truncates `text` to [`CLIPBOARD_MAX_BYTES`] at a char boundary.
///
/// Returns the bounded text plus whether truncation happened. Mirrors the
/// paste-side bound so copies obey the same documented rule (W-135).
fn truncate_to_clipboard_bytes(text: &str) -> (String, bool) {
    if text.len() <= CLIPBOARD_MAX_BYTES {
        return (text.to_string(), false);
    }
    let mut end = CLIPBOARD_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

impl Runtime {
    /// Bumps the search result-set generation, staling prior handles.
    ///
    /// Called on every result-set replacement (set, refresh, clear, and the
    /// PTY-driven auto-refresh). Navigation never calls this.
    pub(super) fn bump_search_result_generation(&mut self) {
        self.search_result_generation = self.search_result_generation.wrapping_add(1);
    }

    /// Mints a handle for the currently bound search, if any.
    ///
    /// `None` when no search is bound (including the unbound test seam that
    /// W-144 retires). Dogfood wrappers use this to thread the bound search
    /// through the host ops while keeping their legacy signatures.
    #[must_use]
    pub fn search_host_handle(&self) -> Option<SearchResultHandle> {
        self.search_view.map(|view| SearchResultHandle {
            view,
            result_generation: self.search_result_generation,
        })
    }

    /// Checks a handle against the current binding, generation, and grid.
    ///
    /// `Denied` on cross-view use, `Stale` when the binding ended or the
    /// generation was superseded, `Unavailable` when the bound View no longer
    /// resolves to a live grid.
    fn check_search_handle(&self, handle: &SearchResultHandle) -> Result<(), HostOpError> {
        let Some(bound) = self.search_view else {
            return Err(HostOpError::Stale(
                "search binding ended; the result set was replaced".to_string(),
            ));
        };
        if bound != handle.view {
            return Err(HostOpError::Denied(
                "handle is bound to another view; cross-view reads are refused".to_string(),
            ));
        }
        if handle.result_generation != self.search_result_generation {
            return Err(HostOpError::Stale(format!(
                "result set was replaced (handle generation {}, current {})",
                handle.result_generation, self.search_result_generation
            )));
        }
        if self.live_view_state(bound).is_none() {
            return Err(HostOpError::Unavailable(
                "bound view no longer resolves to a live grid".to_string(),
            ));
        }
        Ok(())
    }

    /// Builds the bounded query outcome for the currently bound search.
    ///
    /// `None` when the bound grid is gone (the caller then fails closed).
    fn current_host_query(&self, view: ViewId) -> Option<SearchHostQuery> {
        let grid = self.live_view_state(view)?;
        Some(SearchHostQuery {
            handle: SearchResultHandle {
                view,
                result_generation: self.search_result_generation,
            },
            pattern: self.search_state.pattern().to_string(),
            options: self.search_state.options(),
            match_count: self.search_state.match_count(),
            current_index: self.search_state.current_index(),
            grid_generation: grid.generation(),
        })
    }

    /// Bounded search query over one View's grid (stateless read).
    ///
    /// Pure function of `(grid, pattern, options)`: pattern truncated to 256
    /// bytes at a char boundary, results capped at 1000, deterministic
    /// oldest-scrollback-first order. Does not bind, mutate, or bump
    /// anything. `Unavailable` when `view` owns no live grid; never falls
    /// back to another grid.
    pub fn search_host_query(
        &self,
        view: ViewId,
        pattern: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchMatch>, HostOpError> {
        let Some(grid) = self.live_view_state(view) else {
            return Err(HostOpError::Unavailable(format!(
                "view {view:?} owns no live grid"
            )));
        };
        Ok(grid.search(pattern, options))
    }

    /// Bounded per-view snapshot read.
    ///
    /// Returns a copy of exactly `view`'s grid projection (never a mutable
    /// handle, never another view's content). The snapshot carries the
    /// grid's damage generation for caller-side freshness checks.
    /// `Unavailable` when `view` owns no live grid. Returned to the caller
    /// only; never published on the Event Bus.
    pub fn search_host_snapshot(&self, view: ViewId) -> Result<Snapshot, HostOpError> {
        let Some(grid) = self.live_view_state(view) else {
            return Err(HostOpError::Unavailable(format!(
                "view {view:?} owns no live grid"
            )));
        };
        Ok(grid.snapshot())
    }

    /// Binds the search to `view` and computes the bounded result set.
    ///
    /// Replace semantics: any prior set is discarded (coalescing is replace,
    /// never append). Bumps the result generation, staling prior handles.
    /// `Unavailable` when `view` is not a live leaf with a grid; nothing is
    /// bound or mutated on denial.
    pub fn search_host_set_query(
        &mut self,
        view: ViewId,
        pattern: &str,
        options: SearchOptions,
    ) -> Result<SearchHostQuery, HostOpError> {
        if self.live_view_state(view).is_none() {
            return Err(HostOpError::Unavailable(format!(
                "view {view:?} owns no live grid"
            )));
        }
        self.search_view = Some(view);
        {
            let grid = grid_of(&self.pane_sessions, self.primary_view, &self.state, view)
                .ok_or_else(|| {
                    HostOpError::Unavailable(format!("view {view:?} owns no live grid"))
                })?;
            self.search_state.set_search(grid, pattern, options);
        }
        self.bump_search_result_generation();
        self.current_host_query(view)
            .ok_or_else(|| HostOpError::Unavailable(format!("view {view:?} owns no live grid")))
    }

    /// Replaces the bound result set against the bound grid (refresh).
    ///
    /// Preserves the navigation index clamped into the new set (via
    /// `SearchState::refresh`), bumps the generation, and returns a fresh
    /// handle: no result from a prior generation survives. `Stale` when the
    /// handle was superseded; `Denied` on cross-view use. When the bound View
    /// lost its grid the search ends (cleared, generation bumped) and the op
    /// reports `Stale` instead of resolving foreign content.
    pub fn search_host_refresh(
        &mut self,
        handle: &SearchResultHandle,
    ) -> Result<SearchHostQuery, HostOpError> {
        let Some(bound) = self.search_view else {
            return Err(HostOpError::Stale(
                "search binding ended; the result set was replaced".to_string(),
            ));
        };
        if bound != handle.view {
            return Err(HostOpError::Denied(
                "handle is bound to another view; cross-view refresh is refused".to_string(),
            ));
        }
        if handle.result_generation != self.search_result_generation {
            return Err(HostOpError::Stale(format!(
                "result set was replaced (handle generation {}, current {})",
                handle.result_generation, self.search_result_generation
            )));
        }
        let live = self.live_view_state(bound).is_some();
        if !live {
            self.search_clear();
            return Err(HostOpError::Stale(
                "bound view lost its grid; the search ended".to_string(),
            ));
        }
        {
            let grid = grid_of(&self.pane_sessions, self.primary_view, &self.state, bound)
                .ok_or_else(|| {
                    HostOpError::Unavailable("bound view owns no live grid".to_string())
                })?;
            self.search_state.refresh(grid);
        }
        self.bump_search_result_generation();
        self.current_host_query(bound)
            .ok_or_else(|| HostOpError::Unavailable("bound view owns no live grid".to_string()))
    }

    /// Advances the navigation index by `delta` with deterministic wrapping.
    ///
    /// Preserves the result-set generation (moving the current index does not
    /// replace the set). Returns the new current index (`None` when the set
    /// is empty). `Stale`/`Denied` exactly like the other handle ops.
    pub fn search_host_advance(
        &mut self,
        handle: &SearchResultHandle,
        delta: isize,
    ) -> Result<Option<usize>, HostOpError> {
        self.check_search_handle(handle)?;
        self.search_state.advance(delta);
        Ok(self.search_state.current_index())
    }

    /// Computes visible highlights for the bound search in `view`.
    ///
    /// Pure view-local mapping with `is_current` marking, coordinates clipped
    /// to the view's column window. `Denied` when `view` is not the bound
    /// view (a highlight never paints outside its owner's content frame);
    /// `Stale` on a superseded generation. Painting itself stays with the
    /// caller (mechanism only, no presentation).
    pub fn search_host_highlights(
        &self,
        view: &View,
        handle: &SearchResultHandle,
    ) -> Result<Vec<SearchHighlight>, HostOpError> {
        if view.id() != handle.view {
            return Err(HostOpError::Denied(
                "highlight view does not own this result set".to_string(),
            ));
        }
        self.check_search_handle(handle)?;
        let Some(grid) = self.live_view_state(handle.view) else {
            return Err(HostOpError::Unavailable(
                "bound view no longer resolves to a live grid".to_string(),
            ));
        };
        Ok(self.search_state.visible_highlights(view, grid))
    }

    /// Scrolls the bound View minimally to reveal the current match.
    ///
    /// Owns the scroll window only: never mutates Terminal Truth, writes no
    /// PTY bytes, and changes no content geometry. Returns whether the
    /// viewport moved. `Stale` when the target identity no longer resolves
    /// (no jump, no retarget); `Denied` on cross-view use.
    pub fn search_host_scroll_to_current(
        &mut self,
        view: ViewId,
        handle: &SearchResultHandle,
    ) -> Result<bool, HostOpError> {
        self.check_search_handle(handle)?;
        if view != handle.view {
            return Err(HostOpError::Denied(
                "navigation view does not own this result set".to_string(),
            ));
        }
        if self.search_state.current_match().is_none() {
            return Ok(false);
        }
        let grid = grid_of(&self.pane_sessions, self.primary_view, &self.state, view)
            .ok_or_else(|| HostOpError::Unavailable("bound view owns no live grid".to_string()))?;
        let Some(leaf) = self.layout.find_leaf_mut(view) else {
            return Err(HostOpError::Unavailable(
                "bound view left the active layout".to_string(),
            ));
        };
        Ok(self.search_state.scroll_to_current(leaf, grid))
    }

    /// Drives the live selection from the current match.
    ///
    /// The selection is owned by the bound View and follows the single-owner
    /// lifecycle. A pruned or drifted current match fails closed with `Stale`
    /// (live selection cleared so no stale coordinates linger); a valid
    /// history match clears the live selection but keeps its highlight via
    /// [`Self::search_current_persistent_selection`].
    pub fn search_host_apply_selection(
        &mut self,
        handle: &SearchResultHandle,
    ) -> Result<SelectionDriveOutcome, HostOpError> {
        self.check_search_handle(handle)?;
        let grid = self
            .live_view_state(handle.view)
            .ok_or_else(|| HostOpError::Unavailable("bound view owns no live grid".to_string()))?;
        let Some(pers) = self.search_state.current_persistent_selection(grid) else {
            return Ok(SelectionDriveOutcome::NoMatch);
        };
        if !pers.is_valid(grid) {
            self.drop_selection();
            return Err(HostOpError::Stale(
                "current match was pruned or drifted; live selection cleared".to_string(),
            ));
        }
        if let Some(sel) = pers.to_grid_selection(grid) {
            let pin = if sel.active { Some(sel.anchor) } else { None };
            self.install_selection(handle.view, sel, pin, sel.active);
            Ok(SelectionDriveOutcome::Selected)
        } else {
            self.drop_selection();
            Ok(SelectionDriveOutcome::HistoryHighlight)
        }
    }

    /// Installs a selection owned by `view` (selection-drive bridge).
    ///
    /// The single-owner lifecycle applies: installing here replaces any live
    /// selection. The range is clamped and wide-snapped against the owner's
    /// grid (identity for in-bounds selections) so a split pair is never
    /// installed. `Unavailable` when `view` owns no live grid; nothing is
    /// installed on denial.
    pub fn search_host_install_selection(
        &mut self,
        view: ViewId,
        selection: Selection,
        anchor_press: Option<CellPos>,
        dragging: bool,
    ) -> Result<(), HostOpError> {
        let snapshot = self.live_view_state(view).map(|grid| grid.snapshot());
        let Some(snapshot) = snapshot else {
            return Err(HostOpError::Unavailable(format!(
                "view {view:?} owns no live grid"
            )));
        };
        let settled = selection.clamped(&snapshot).snapped(Some(&snapshot));
        self.install_selection(view, settled, anchor_press, dragging);
        Ok(())
    }

    /// Lifts the live selection owned by `view` to a fenced handle.
    ///
    /// `Denied` when the live selection is owned by another view (one live
    /// selection; starting elsewhere replaces it — never re-expressed against
    /// the wrong grid). `Unavailable` when no selection exists or the owner
    /// grid is gone.
    pub fn search_host_persist_live_selection(
        &self,
        view: ViewId,
    ) -> Result<SelectionHandle, HostOpError> {
        let Some((sel, grid)) = self.selection_owner_state() else {
            return Err(HostOpError::Unavailable(
                "no live selection on a live grid".to_string(),
            ));
        };
        if sel.owner != view {
            return Err(HostOpError::Denied(
                "live selection is owned by another view".to_string(),
            ));
        }
        Ok(SelectionHandle {
            view,
            epoch: grid.buffer_epoch(),
            evicted_at: grid.scrollback_evicted_total(),
            selection: PersistentSelection::from_grid_selection(sel.selection, grid),
        })
    }

    /// Restores a fenced selection handle into the live-grid selection.
    ///
    /// `Ok(true)` when the buffer rows still map into the live window,
    /// `Ok(false)` when they validly moved into history (live selection
    /// cleared, buffer text still readable via the handle's selection).
    /// Fail-closed `Stale` when the grid's buffer epoch changed since mint
    /// (scrollback clear, reset, resize/reflow: the rows no longer address
    /// the same content), when prune drift evicted the anchor past the
    /// window, or when the line identity no longer resolves — with the live
    /// selection cleared so no stale coordinates linger. `Denied` on
    /// cross-view use, `Unavailable` when the view's grid is gone.
    pub fn search_host_restore_selection(
        &mut self,
        handle: &SelectionHandle,
    ) -> Result<bool, HostOpError> {
        let grid = self.live_view_state(handle.view).ok_or_else(|| {
            HostOpError::Unavailable(format!("view {:?} owns no live grid", handle.view))
        })?;
        if grid.buffer_epoch() != handle.epoch {
            self.drop_selection_owned_by(handle.view);
            return Err(HostOpError::Stale(
                "buffer epoch changed since the handle was minted \
                 (scrollback clear, reset, or resize/reflow); \
                 live selection cleared"
                    .to_string(),
            ));
        }
        // Prune shifts combined-buffer rows arithmetically without changing
        // content identity: subtract the drift exactly (the `zone_buffer_row`
        // precedent). Only anchors evicted past row zero stale.
        let drift = grid
            .scrollback_evicted_total()
            .checked_sub(handle.evicted_at)
            .ok_or_else(|| {
                HostOpError::Stale("scrollback eviction clock went backwards".to_string())
            })? as usize;
        let shift = |pos: BufferPos| {
            pos.buffer_row
                .checked_sub(drift)
                .map(|row| BufferPos::new(row, pos.col))
        };
        let (Some(anchor), Some(focus)) = (
            shift(handle.selection.anchor),
            shift(handle.selection.focus),
        ) else {
            self.drop_selection_owned_by(handle.view);
            return Err(HostOpError::Stale(
                "scrollback prune evicted the anchored lines; live selection cleared".to_string(),
            ));
        };
        let adjusted = PersistentSelection {
            anchor,
            focus,
            ..handle.selection
        };
        if !adjusted.is_valid(grid) {
            self.drop_selection_owned_by(handle.view);
            return Err(HostOpError::Stale(
                "anchored line identity no longer resolves; live selection cleared".to_string(),
            ));
        }
        match adjusted.to_grid_selection(grid) {
            Some(sel) => {
                let pin = if sel.active { Some(sel.anchor) } else { None };
                self.install_selection(handle.view, sel, pin, sel.active);
                Ok(true)
            }
            None => {
                self.drop_selection();
                Ok(false)
            }
        }
    }

    /// Gated clipboard write behind the existing `clipboard.write` grant.
    ///
    /// `TrustedUser` (Core search/copy-mode and mouse gestures) writes
    /// directly, exactly as today. `Plugin` must hold `clipboard.write` for
    /// its manifest hash; denial writes nothing (no partial write) and names
    /// the missing capability. Payloads are char-boundary truncated to
    /// `CLIPBOARD_MAX_BYTES` (8192) with a `truncated` flag (W-135); empty
    /// input performs no write. System failures stay fail-soft (recorded for
    /// `last_clipboard_error`, headless buffers still update) like the legacy
    /// paths. No new capability identifier is introduced.
    pub fn search_host_copy_to_clipboard(
        &mut self,
        caller: ClipboardCaller<'_>,
        text: &str,
    ) -> Result<YankOutcome, HostOpError> {
        if let ClipboardCaller::Plugin {
            plugin_id,
            manifest_hash,
        } = caller
        {
            let need = CapabilityId::parse("clipboard.write").map_err(|err| {
                HostOpError::Denied(format!("unparseable clipboard capability: {err}"))
            })?;
            if !self.is_capability_granted(plugin_id, manifest_hash, &need) {
                return Err(HostOpError::Denied(format!(
                    "plugin '{}' lacks capability 'clipboard.write' for hash '{manifest_hash}' (deny-by-default); no bytes written",
                    plugin_id.as_str()
                )));
            }
        }
        if text.is_empty() {
            return Ok(YankOutcome {
                text: String::new(),
                truncated: false,
            });
        }
        let (bounded, truncated) = truncate_to_clipboard_bytes(text);
        match self.clipboard.set_text(bounded.clone()) {
            Ok(()) => self.clear_clipboard_error(),
            Err(err) => self.record_clipboard_error(err),
        }
        Ok(YankOutcome {
            text: bounded,
            truncated,
        })
    }

    /// Gated yank of the current live selection (clipboard plus primary).
    ///
    /// Reads the owner's grid (never another grid), gated-writes the text,
    /// and mirrors the platform primary selection exactly like the legacy
    /// yank paths (`copy_selection_to_clipboard` plus primary). `Ok(None)`
    /// when no non-empty selection exists. Denial writes nothing.
    pub fn search_host_yank_selection(
        &mut self,
        caller: ClipboardCaller<'_>,
    ) -> Result<Option<YankOutcome>, HostOpError> {
        let Some(text) = self.selection_text() else {
            return Ok(None);
        };
        let outcome = self.search_host_copy_to_clipboard(caller, &text)?;
        match self.clipboard.set_primary(outcome.text.clone()) {
            Ok(()) => self.clear_clipboard_error(),
            Err(err) => self.record_clipboard_error(err),
        }
        Ok(Some(outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_plugin_host::{CapabilityId, GrantRecord, PluginId};
    use std::collections::BTreeSet;

    fn headless_runtime() -> Runtime {
        let mut rt = Runtime::with_defaults().expect("headless runtime must build");
        rt.force_headless_clipboard();
        rt
    }

    fn feed_text(rt: &mut Runtime, text: &str) {
        rt.handle_pty_bytes(text.as_bytes());
    }

    fn bound_search(rt: &mut Runtime, pattern: &str) -> (ViewId, SearchHostQuery) {
        let view = rt.focused_view().expect("default layout has focus");
        let query = rt
            .search_host_set_query(view, pattern, SearchOptions::default())
            .expect("default view owns a live grid");
        assert!(query.match_count > 0, "fixture must match");
        (view, query)
    }

    #[test]
    fn host_set_query_binds_view_and_reports_bounds() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "hello world hello");
        let view = rt.focused_view().expect("focus");
        let query = rt
            .search_host_set_query(view, "hello", SearchOptions::default())
            .expect("set binds");
        assert_eq!(query.handle.view, view);
        assert_eq!(query.pattern, "hello");
        assert_eq!(query.match_count, 2);
        assert_eq!(query.current_index, Some(0));
        assert_eq!(rt.search_view(), Some(view));
    }

    #[test]
    fn host_query_is_stateless_and_bounded() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "alpha beta alpha");
        let view = rt.focused_view().expect("focus");
        assert!(rt.search_view().is_none(), "stateless query binds nothing");
        let matches = rt
            .search_host_query(view, "alpha", SearchOptions::new(true, 1))
            .expect("query reads");
        assert_eq!(matches.len(), 1);
        assert!(rt.search_view().is_none(), "still unbound after query");
        let overlong = "q".repeat(512);
        let bounded = rt
            .search_host_query(view, &overlong, SearchOptions::default())
            .expect("overlong query is truncated, not rejected");
        assert!(bounded.is_empty(), "truncated pattern matches nothing");
    }

    #[test]
    fn unknown_view_fails_closed_without_fallback() {
        let rt = headless_runtime();
        let ghost = ViewId::new(9999);
        assert!(matches!(
            rt.search_host_query(ghost, "x", SearchOptions::default()),
            Err(HostOpError::Unavailable(_))
        ));
        assert!(matches!(
            rt.search_host_snapshot(ghost),
            Err(HostOpError::Unavailable(_))
        ));
    }

    #[test]
    fn stale_generation_fails_closed_after_replacement() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "needle here and needle there");
        let (_view, first) = bound_search(&mut rt, "needle");
        let view = first.handle.view;
        // A second set replaces the set and bumps the generation.
        let second = rt
            .search_host_set_query(view, "needle", SearchOptions::default())
            .expect("re-set binds")
            .handle;
        assert_ne!(first.handle.result_generation, second.result_generation);
        // The prior handle is stale everywhere it matters.
        assert!(matches!(
            rt.search_host_advance(&first.handle, 1),
            Err(HostOpError::Stale(_))
        ));
        let bound_view = rt.layout.find_leaf(view).expect("bound leaf live");
        assert!(matches!(
            rt.search_host_highlights(bound_view, &first.handle),
            Err(HostOpError::Stale(_))
        ));
        assert!(matches!(
            rt.search_host_scroll_to_current(view, &first.handle),
            Err(HostOpError::Stale(_))
        ));
        assert!(matches!(
            rt.search_host_apply_selection(&first.handle),
            Err(HostOpError::Stale(_))
        ));
        assert!(matches!(
            rt.search_host_refresh(&first.handle),
            Err(HostOpError::Stale(_))
        ));
        // The fresh handle still works.
        assert!(rt.search_host_advance(&second, 1).is_ok());
    }

    #[test]
    fn refresh_replaces_and_stales_prior_handle() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "needle one");
        let (_view, first) = bound_search(&mut rt, "needle");
        // Output on the bound grid auto-refreshes: the set is replaced and
        // the pre-output handle is already stale.
        feed_text(&mut rt, "needle two");
        assert!(
            matches!(
                rt.search_host_advance(&first.handle, 1),
                Err(HostOpError::Stale(_))
            ),
            "PTY output replaces the result set"
        );
        // A freshly minted handle observes the replaced set; an explicit
        // refresh replaces it again and stales the fresh handle in turn.
        let fresh = rt.search_host_handle().expect("binding survives output");
        let refreshed = rt.search_host_refresh(&fresh).expect("refresh replaces");
        assert_ne!(refreshed.handle.result_generation, fresh.result_generation);
        assert_eq!(refreshed.match_count, 2);
        assert!(matches!(
            rt.search_host_advance(&fresh, 1),
            Err(HostOpError::Stale(_))
        ));
        assert!(rt.search_host_advance(&refreshed.handle, 1).is_ok());
    }

    #[test]
    fn clear_ends_binding_and_stales_handles() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "needle here");
        let (_view, query) = bound_search(&mut rt, "needle");
        rt.search_clear();
        assert!(matches!(
            rt.search_host_advance(&query.handle, 1),
            Err(HostOpError::Stale(_))
        ));
    }

    #[test]
    fn cross_view_highlight_is_denied_not_empty() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "needle here");
        let (_view, query) = bound_search(&mut rt, "needle");
        let other = View::new(ViewId::new(4242), 80, 24);
        assert!(matches!(
            rt.search_host_highlights(&other, &query.handle),
            Err(HostOpError::Denied(_))
        ));
        assert!(matches!(
            rt.search_host_scroll_to_current(other.id(), &query.handle),
            Err(HostOpError::Denied(_))
        ));
    }

    #[test]
    fn scroll_and_apply_drive_bound_selection() {
        let mut rt = headless_runtime();
        for i in 0..30 {
            feed_text(&mut rt, &format!("line{i:02} needle\n"));
        }
        feed_text(&mut rt, "live needle here");
        let (view, query) = bound_search(&mut rt, "needle");
        let handle = query.handle;
        // The head match is scrollback history: highlight-persistent but not
        // live-selectable, and the live selection stays untouched.
        let outcome = rt
            .search_host_apply_selection(&handle)
            .expect("apply drives");
        assert_eq!(outcome, SelectionDriveOutcome::HistoryHighlight);
        assert!(rt.selection().is_none());
        // Navigate to the live-grid match: the same handle keeps working
        // because navigation preserves the result-set generation.
        let last = query.match_count - 1;
        let idx = rt
            .search_host_advance(&handle, last as isize)
            .expect("advance works");
        assert_eq!(idx, Some(last));
        let outcome = rt
            .search_host_apply_selection(&handle)
            .expect("apply again");
        assert_eq!(outcome, SelectionDriveOutcome::Selected);
        assert_eq!(rt.selection_owner(), Some(view));
        let text = rt.selection_text().expect("selection text reads");
        assert_eq!(text, "needle");
        // The scrolled reveal is a no-op success when already visible.
        let scrolled = rt
            .search_host_scroll_to_current(view, &handle)
            .expect("scroll works");
        assert!(!scrolled, "live match is already visible");
    }

    #[test]
    fn erase_stales_persistent_restore() {
        let mut rt = headless_runtime();
        let view = rt.focused_view().expect("focus");
        feed_text(&mut rt, "victim line\r\n");
        let sel = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 5));
        assert!(rt.set_view_selection(view, sel), "live selection sets");
        let handle = rt
            .search_host_persist_live_selection(view)
            .expect("persist works");
        // ED 3 clears scrollback history and bumps the buffer epoch: the
        // anchored rows no longer address the same content, so the restore
        // fails closed with `Stale` instead of resolving into new content.
        feed_text(&mut rt, "\x1b[3J");
        assert!(matches!(
            rt.search_host_restore_selection(&handle),
            Err(HostOpError::Stale(_))
        ));
        assert!(
            rt.selection().is_none(),
            "stale restore leaves no stale coordinates"
        );
    }

    #[test]
    fn prune_drift_adjusts_but_eviction_stales_restore() {
        use crate::config::RuntimeConfig;
        // Tiny scrollback so a few lines force oldest-first eviction.
        let mut rt = Runtime::new(RuntimeConfig {
            scrollback: 4,
            ..RuntimeConfig::default()
        })
        .expect("headless runtime must build");
        rt.force_headless_clipboard();
        let view = rt.focused_view().expect("focus");
        // Fill the grid, then add a late marker and scroll just enough that
        // the grid is full with a small retained scrollback (no eviction
        // yet, so the marker is live-anchored at a high buffer row).
        for i in 0..20 {
            feed_text(&mut rt, &format!("filler {i}\r\n"));
        }
        feed_text(&mut rt, "marker line\r\n");
        for i in 0..6 {
            feed_text(&mut rt, &format!("pad {i}\r\n"));
        }
        let marker_row = {
            let grid = rt.live_view_state(view).expect("live grid");
            let m = grid
                .search("marker", SearchOptions::default())
                .pop()
                .expect("marker retained and live");
            assert!(
                m.line_id.is_none(),
                "marker must be live-anchored for this test"
            );
            m.buffer_row
        };
        let grid_row = {
            let grid = rt.live_view_state(view).expect("live grid");
            (marker_row - grid.scrollback_len()) as u16
        };
        let sel = Selection::simple(CellPos::new(grid_row, 0), CellPos::new(grid_row, 5));
        assert!(rt.set_view_selection(view, sel), "live selection sets");
        let handle = rt
            .search_host_persist_live_selection(view)
            .expect("persist works");
        // Scroll the marker into history with some prune, but keep the
        // marker retained: drift is subtracted arithmetically, so the
        // restore reports history (`Ok(false)`) instead of staling. The
        // drift must exceed `marker_row - sb_cap` so the adjusted row lands
        // behind the live window.
        let want_drift = marker_row.saturating_sub(3);
        for i in 0..40 {
            feed_text(&mut rt, &format!("drift {i}\r\n"));
            let (drift, retained) = {
                let grid = rt.live_view_state(view).expect("live grid");
                (
                    grid.scrollback_evicted_total()
                        .saturating_sub(handle.evicted_at),
                    !grid.search("marker", SearchOptions::default()).is_empty(),
                )
            };
            if drift as usize >= want_drift && retained {
                break;
            }
            assert!(i < 39, "marker must survive into a drifted history");
        }
        assert_eq!(
            rt.search_host_restore_selection(&handle),
            Ok(false),
            "drift-adjusted history restores as history, not stale"
        );
        // Evict the marker past the retained window: now the anchor addresses
        // evicted content and the restore fails closed.
        for i in 0..40 {
            feed_text(&mut rt, &format!("evictor {i}\r\n"));
            let retained = {
                let grid = rt.live_view_state(view).expect("live grid");
                !grid.search("marker", SearchOptions::default()).is_empty()
            };
            if !retained {
                break;
            }
            assert!(i < 39, "marker must eventually prune");
        }
        assert!(matches!(
            rt.search_host_restore_selection(&handle),
            Err(HostOpError::Stale(_))
        ));
        assert!(
            rt.selection().is_none(),
            "stale restore leaves no stale coordinates"
        );
    }

    #[test]
    fn persist_denies_cross_view_use() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "hello world");
        let view = rt.focused_view().expect("focus");
        let sel = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
        rt.set_view_selection(view, sel);
        let other = ViewId::new(7777);
        assert!(matches!(
            rt.search_host_persist_live_selection(other),
            Err(HostOpError::Denied(_))
        ));
        // The owning view persists fine.
        assert!(rt.search_host_persist_live_selection(view).is_ok());
    }

    #[test]
    fn layout_change_ends_binding_and_stales_handles() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "needle here");
        let (_view, query) = bound_search(&mut rt, "needle");
        // Replace the layout with a fresh leaf: the lifecycle funnel ends the
        // search bound to the gone view, so prior handles stale (fail closed)
        // instead of addressing a dead grid.
        let fresh = View::new(ViewId::new(555), 80, 24);
        rt.set_layout(LayoutNode::leaf(fresh));
        assert!(
            rt.search_view().is_none(),
            "binding ends with the bound view"
        );
        assert!(matches!(
            rt.search_host_advance(&query.handle, 1),
            Err(HostOpError::Stale(_))
        ));
    }

    #[test]
    fn plugin_clipboard_write_without_grant_is_denied_without_write() {
        let mut rt = headless_runtime();
        let plugin_id = PluginId::new("test.search-plugin").expect("valid id");
        let before = rt.clipboard.headless_contents().to_string();
        let err = rt
            .search_host_copy_to_clipboard(
                ClipboardCaller::Plugin {
                    plugin_id: &plugin_id,
                    manifest_hash: "hash-1",
                },
                "sneaky write",
            )
            .expect_err("missing grant denies");
        assert!(matches!(err, HostOpError::Denied(_)));
        assert_eq!(rt.clipboard.headless_contents(), before);
        // And yanking denies the same way with no partial write.
        feed_text(&mut rt, "hello world");
        let view = rt.focused_view().expect("focus");
        rt.set_view_selection(
            view,
            Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4)),
        );
        let err = rt
            .search_host_yank_selection(ClipboardCaller::Plugin {
                plugin_id: &plugin_id,
                manifest_hash: "hash-1",
            })
            .expect_err("yank without grant denies");
        assert!(matches!(err, HostOpError::Denied(_)));
        assert_eq!(rt.clipboard.headless_contents(), before);
    }

    #[test]
    fn granted_plugin_write_succeeds_with_existing_capability() {
        use bitty_plugin_host::{
            CapabilityRequests, Compat, LazyTriggers, PluginIdentity, PluginManifest,
        };
        let mut rt = headless_runtime();
        let plugin_id = PluginId::new("test.search-plugin").expect("valid id");
        // The grant intersects the manifest's declared capabilities, so the
        // fixture plugin must declare `clipboard.write` first.
        let manifest = PluginManifest {
            identity: PluginIdentity {
                id: plugin_id.clone(),
                name: "Test".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            provided_services: Vec::new(),
            required_services: Vec::new(),
            capabilities: CapabilityRequests {
                ids: [CapabilityId::parse("clipboard.write").expect("known capability")]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            tools: Vec::new(),
            network: Vec::new(),
            limits: Default::default(),
            lazy: LazyTriggers {
                commands: Vec::new(),
                events: Vec::new(),
                claims: Vec::new(),
            },
            raw_bytes_len: 256,
        };
        rt.register_plugin(manifest).expect("fixture registers");
        let mut granted = BTreeSet::new();
        granted.insert(CapabilityId::parse("clipboard.write").expect("known capability"));
        rt.insert_grant(GrantRecord::granted(
            plugin_id.clone(),
            "hash-1",
            granted,
            1,
        ));
        // No new capability identifier: the existing `clipboard.write` gates.
        let outcome = rt
            .search_host_copy_to_clipboard(
                ClipboardCaller::Plugin {
                    plugin_id: &plugin_id,
                    manifest_hash: "hash-1",
                },
                "plugin yank",
            )
            .expect("granted write succeeds");
        assert!(!outcome.truncated);
        assert_eq!(outcome.text, "plugin yank");
        assert_eq!(rt.clipboard.headless_contents(), "plugin yank");
    }

    #[test]
    fn trusted_user_write_truncates_over_limit_with_flag() {
        let mut rt = headless_runtime();
        let long = "y".repeat(CLIPBOARD_MAX_BYTES + 64);
        let outcome = rt
            .search_host_copy_to_clipboard(ClipboardCaller::TrustedUser, &long)
            .expect("trusted write succeeds");
        assert!(outcome.truncated, "over-limit copy truncates with a flag");
        assert_eq!(outcome.text.len(), CLIPBOARD_MAX_BYTES);
        assert!(outcome.text.is_char_boundary(outcome.text.len()));
        assert_eq!(rt.clipboard.headless_contents(), outcome.text);
        // Empty input performs no write.
        let outcome = rt
            .search_host_copy_to_clipboard(ClipboardCaller::TrustedUser, "")
            .expect("empty write is a no-op success");
        assert_eq!(outcome.text, "");
        assert!(!outcome.truncated);
    }

    #[test]
    fn install_bridge_is_view_bound() {
        let mut rt = headless_runtime();
        feed_text(&mut rt, "hello world");
        let view = rt.focused_view().expect("focus");
        let sel = Selection::simple(CellPos::new(0, 0), CellPos::new(0, 4));
        rt.search_host_install_selection(view, sel, None, false)
            .expect("install on live grid");
        assert_eq!(rt.selection_owner(), Some(view));
        assert_eq!(rt.selection_text().as_deref(), Some("hello"));
        let ghost = ViewId::new(31337);
        assert!(matches!(
            rt.search_host_install_selection(ghost, sel, None, false),
            Err(HostOpError::Unavailable(_))
        ));
    }
}
