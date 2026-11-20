#![forbid(unsafe_code)]
//! Plugin-facing panel content presentation (`bitty.ui.panel.*`, bitty#2388,
//! CTX-1909).
//!
//! Contract source: `specifications/panel-gui-content-presentation-candidate.md`
//! in `bitty-terminal-docs` (draft candidate, decision-ready), sections
//! "Recommended API shape (candidate v2)", "Per-placement and limits", and
//! "Focus, close, and fail-closed paths".
//!
//! The Core swap this module exposes already landed (CTX-1895):
//! [`Runtime::present_panel_content`](Runtime::present_panel_content) changes a
//! terminal-backed panel's content kind, freezes the displaced terminal's
//! geometry, and [`Runtime::release_panel_content`](Runtime::release_panel_content)
//! restores it. This module adds the *plugin* half the record also asks for:
//! a host-derived handle, the owner identity the Core record deliberately does
//! not carry, the recorded placement hint, the typed refusals the record
//! names, the **Core-owned bounded key queue** with its owner-polled drain
//! (CTX-1911, `bitty.ui.panel.poll`), and the two observation-only
//! `panel.content.*` events.
//!
//! # What this layer owns, and what it does not
//!
//! | Concern | Owner |
//! |---|---|
//! | Which panel the caller presents into | **Core**, resolved from focus. The plugin supplies no `PanelId`, `ViewId`, coordinate, size, or surface handle |
//! | The content swap, the displaced record, the geometry freeze, the input gate | Core (CTX-1895), untouched here |
//! | The handle a plugin holds, its `PluginId`/generation, the placement hint | this module ([`UiPanelLayerRecord`]) |
//! | The key queue of a presented layer | this module ([`PanelContentInputQueue`]): Core appends on the input path, the owner drains with `poll` on its own cold path |
//! | The `panel.content.released` / `panel.content.input-available` observations | this module ([`PanelContentEvent`]); delivery to the bus is `PluginRuntime::pump_panel_content_events` |
//! | Whether a placement is *applied* | nobody: the hint is **recorded, not applied** in v1 (record "hint only") |
//!
//! # Input routing
//!
//! While a layer is presented, the runtime's two keyboard entry points swallow
//! the key instead of encoding it (`runtime::input`). The owned path hands the
//! key here (`Runtime::capture_panel_content_key`); the borrowed path
//! (`handle_key_event_ref`) is a second copy of the same gate and reaches the
//! same queue through the same call. Nothing on that path runs plugin code, and
//! no byte reaches the displaced PTY.
//!
//! The queue is per layer, capped at [`UI_PANEL_INPUT_QUEUE_MAX`] events of at
//! most [`UI_PANEL_INPUT_TEXT_MAX_BYTES`] bytes, with drop-oldest overflow and
//! a sticky flag. It is dropped the moment the layer ends, so no key survives
//! into a panel somebody else owns.
//!
//! # Typed refusals
//!
//! Every refusal happens before any mutation and uses the record's spelling:
//!
//! | Case | Code |
//! |---|---|
//! | the focused panel already presents another owner's layer | [`E_UI_PANEL_BUSY`] |
//! | this generation already presents | [`E_UI_PANEL_NESTED`] |
//! | no focused panel, no panel there, or not terminal-backed | [`E_UI_PANEL_UNAVAILABLE`] |
//! | no panel host installed (headless/no-surface host) | [`E_UI_UNAVAILABLE`] |
//! | a stale, foreign, previous-generation, or already-released handle | [`E_UI_NOT_OWNER`] |
//! | an unusable scene | `E_UI_COMPONENT_INVALID` |
//!
//! # Handle lifetime
//!
//! A handle dies with its layer. The Core record is released by exactly three
//! paths — the owner's token release, the generation edge, and panel disposal —
//! and this module's entry is pruned on the first two
//! ([`Runtime::release_panel_content`] and
//! [`Runtime::restore_released_panel_presentation`]), so a handle can never
//! outlive what it names. A handle whose Core record is gone answers
//! [`E_UI_NOT_OWNER`] (and drops its entry) instead of restoring a terminal
//! somebody else now owns.
//!
//! The fence compares the owner **and** the generation, because one edge can
//! leave a row behind: `RuntimePanelContentBackend::release_owner`
//! ([`crate::plugin_runtime::RuntimePanelContentBackend`]) reports zero instead
//! of releasing when the shared runtime is borrowed by an in-flight call, so a
//! dying generation's row can survive its teardown. The next generation must
//! adopt nothing from it: a handle the row names is not the successor's, and
//! neither the row's own generation nor the successor may release a layer the
//! other owns.

use bitty_lua::{
    BridgeError, E_UI_NOT_OWNER, E_UI_PANEL_BUSY, E_UI_PANEL_NESTED, E_UI_PANEL_UNAVAILABLE,
    E_UI_UNAVAILABLE, OverlayInput, OverlayKeyChord, PanelPoll, UI_PANEL_INPUT_QUEUE_MAX,
    UI_PANEL_INPUT_TEXT_MAX_BYTES, UiPanelSpec,
};
use bitty_plugin_host::manifest::PluginId;
use bitty_rich::scene::{BlockId, Scene};
use bitty_ui::ViewId;
use bitty_ui::panel::PanelId;

use super::Runtime;
use crate::registry::{PanelContentToken, PanelError, ViewContent};

/// Closed release reason Core records when it releases a plugin layer outside
/// the owner's request: the panel closed, was disposed, or the app drove the
/// token release (bitty#2388 "Release reasons are a closed vocabulary").
pub const PANEL_CONTENT_RELEASE_CLOSED: &str = "closed";

/// Closed release reason for a layer dropped when its generation ended without
/// the owner's release reaching the runtime (the generation port carries the
/// displaced record, not the cause).
pub const PANEL_CONTENT_RELEASE_UNLOADED: &str = "unloaded";

/// Maximum pending `panel.content.*` observations buffered between drains.
///
/// The application drains observations on its cold tick, so an owner that
/// present/releases in a loop inside one callback would otherwise grow the
/// buffer without bound. Overflow drops the oldest observation and counts it,
/// mirroring `PENDING_RELEASED_MAX` in `plugin_runtime::overlay`, the
/// focusable-overlay capture's identical bound.
pub const PANEL_CONTENT_EVENT_QUEUE_MAX: usize = 64;

/// How many released handles are remembered for a post-release `poll`.
///
/// `bitty.ui.panel.poll` answers `released` (with the reason) for the owner of
/// a layer Core ended, exactly like the overlay capture's remembered release.
/// The memory is bounded and evicted oldest-first, which is also handle order:
/// handles increase monotonically and are never reused.
pub const PANEL_CONTENT_RELEASE_MEMORY_MAX: usize = 64;

/// Core-owned bounded key queue of one presented layer (bitty#2388 D7).
///
/// The queue belongs to the layer, not to the plugin: Core appends on the
/// input path and the plugin drains it on its own cold path through
/// `bitty.ui.panel.poll`. Nothing here calls into a VM, so the input hot path
/// gains one `push_back` and no plugin code (`P0-AC-015`).
///
/// Bounds: [`UI_PANEL_INPUT_QUEUE_MAX`] events, [`UI_PANEL_INPUT_TEXT_MAX_BYTES`]
/// bytes each, drop-oldest overflow with a sticky flag. The events are the
/// same bounded key payload the focusable overlay capture queues
/// ([`OverlayInput`]), so one key channel has one shape.
#[derive(Debug, Default)]
pub struct PanelContentInputQueue {
    events: std::collections::VecDeque<OverlayInput>,
    next_sequence: u64,
    last_delivered: u64,
    dropped: u64,
    overflowed: bool,
}

impl PanelContentInputQueue {
    /// An empty queue for one freshly presented layer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of queued (undrained) events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no event is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Queued events dropped by the drop-oldest overflow bound.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Sticky overflow flag (`true` once any event was dropped).
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Sequence of the last event handed to the owner, `0` before any drain.
    #[must_use]
    pub const fn last_delivered(&self) -> u64 {
        self.last_delivered
    }

    /// Append one key event, dropping the oldest when the queue is full.
    ///
    /// The text is truncated to [`UI_PANEL_INPUT_TEXT_MAX_BYTES`] at a `char`
    /// boundary, so a single event can never exceed the record's payload
    /// bound. The chord rides alongside the flat text exactly like the overlay
    /// capture's (issue #2098): without it `ctrl+enter` and a bare `enter`
    /// marshal identically.
    pub fn push_key(&mut self, text: &str, chord: Option<OverlayKeyChord>) {
        if self.events.len() >= UI_PANEL_INPUT_QUEUE_MAX {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            self.overflowed = true;
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push_back(OverlayInput {
            sequence: self.next_sequence,
            kind: "key".to_string(),
            text: bounded_panel_text(text),
            chord: chord.filter(OverlayKeyChord::is_modified),
        });
    }

    /// Drain up to `max` queued events, oldest first.
    fn drain(&mut self, max: usize) -> Vec<OverlayInput> {
        let take = max.min(self.events.len());
        let drained: Vec<OverlayInput> = self.events.drain(..take).collect();
        if let Some(last) = drained.last() {
            self.last_delivered = last.sequence;
        }
        drained
    }
}

/// Truncate one key text to the record's per-event bound at a `char` boundary.
fn bounded_panel_text(text: &str) -> String {
    if text.len() <= UI_PANEL_INPUT_TEXT_MAX_BYTES {
        return text.to_string();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let width = ch.len_utf8();
        if used + width > UI_PANEL_INPUT_TEXT_MAX_BYTES {
            break;
        }
        used += width;
        out.push(ch);
    }
    out
}

/// Truncate a release reason to the closed vocabulary's byte bound.
fn bounded_reason(reason: &str) -> String {
    bounded_panel_text(reason)
}

/// One observation-only `panel.content.*` event the runtime produced
/// (bitty#2388 D8, CTX-1911).
///
/// The runtime is the producer and owns the bounded buffer; the delivery call
/// site turns each observation into the closed bus kind named by
/// [`PanelContentEvent::kind`]. Neither variant carries content, key bytes, or
/// terminal state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelContentEvent {
    /// A layer ended. `reason` is the closed release vocabulary.
    Released {
        /// Plugin that presented the layer.
        owner: String,
        /// Opaque layer handle the owner held.
        handle: i64,
        /// Closed release reason Core recorded.
        reason: String,
    },
    /// A presented layer has undrained queued input (level-triggered nudge).
    InputAvailable {
        /// Plugin that presented the layer.
        owner: String,
        /// Opaque layer handle the owner holds.
        handle: i64,
        /// Queued event count at observation time.
        queued: usize,
    },
}

impl PanelContentEvent {
    /// Closed bus kind this observation is delivered as.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Released { .. } => "panel.content.released",
            Self::InputAvailable { .. } => "panel.content.input-available",
        }
    }

    /// Plugin the observation belongs to.
    #[must_use]
    pub fn owner(&self) -> &str {
        match self {
            Self::Released { owner, .. } | Self::InputAvailable { owner, .. } => owner,
        }
    }

    /// Layer handle the observation names.
    #[must_use]
    pub const fn handle(&self) -> i64 {
        match self {
            Self::Released { handle, .. } | Self::InputAvailable { handle, .. } => *handle,
        }
    }
}

/// Remembered release of one handle, for the owner's next `poll`.
///
/// Mirrors the overlay capture's remembered release: the owner that asked
/// (`released`) or was ended by Core (`closed`, `suspended`, ...) can read the
/// outcome once, then the handle is dead for every verb. Bounded by
/// [`PANEL_CONTENT_RELEASE_MEMORY_MAX`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPanelReleaseRecord {
    /// Plugin that presented the layer.
    pub owner: PluginId,
    /// Activation generation that presented it.
    pub generation: u32,
    /// Closed release reason.
    pub reason: String,
    /// Whether the layer's queue had overflowed before it was dropped.
    pub overflowed: bool,
    /// Sequence of the last event the owner drained from the layer.
    pub seq: u64,
}

/// One layer presented through `bitty.ui.panel.present`.
///
/// The row exists for the three facts the Core record does not hold: who
/// presented (owner + activation generation), the handle the caller was given,
/// and the placement hint the caller asked for. Everything about the swap
/// itself lives in [`crate::registry::PresentedPanelContent`], which stays the
/// single restore authority — this row never restores anything on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPanelLayerRecord {
    /// Host-derived handle the caller holds. Never supplied by the plugin, never
    /// reused after release.
    pub handle: i64,
    /// Plugin that presented the layer.
    pub owner: PluginId,
    /// Activation generation that presented it. One generation presents at most
    /// one layer (`E_UI_PANEL_NESTED` otherwise).
    pub generation: u32,
    /// Panel container the layer was presented on, resolved from focus at
    /// present time.
    pub panel: PanelId,
    /// Attachment view of that panel.
    pub view: ViewId,
    /// The Core record's opaque token: the only value that can release the
    /// layer, held here so the caller never sees it.
    pub token: PanelContentToken,
    /// The validated descriptor exactly as the caller sent it, recorded and
    /// **not applied** (`placement` is a hint in v1).
    pub spec: UiPanelSpec,
}

impl Runtime {
    /// Presents `node` as the focused panel's content and returns the
    /// caller's handle (bitty#2388 `bitty.ui.panel.present`).
    ///
    /// The target is resolved here and nowhere else: the focused leaf, whose
    /// panel must be terminal-backed. The plugin cannot name a panel, a
    /// coordinate, a size, or a surface, and the displaced terminal keeps
    /// running hidden exactly as the Core swap already guarantees.
    ///
    /// `spec` is recorded on the returned row and the placement hint is not
    /// applied: the panel keeps the placement the user gave it, which is the
    /// record's "hint only" rule, not an omission.
    ///
    /// # Errors
    ///
    /// [`E_UI_UNAVAILABLE`] when no panel host is installed or the shared host
    /// is borrowed (reentrant plugin call); [`E_UI_PANEL_NESTED`] when this
    /// generation already presents; [`E_UI_PANEL_BUSY`] when the panel already
    /// presents another owner's layer; [`E_UI_PANEL_UNAVAILABLE`] when there is
    /// no focused panel, the panel is not terminal-backed, or it has no present
    /// frame; `E_UI_COMPONENT_INVALID` when the scene is refused by admission.
    /// Every refusal changes nothing: no record, no hint, no handle, no freeze.
    pub fn ui_panel_present(
        &mut self,
        owner: &PluginId,
        generation: u32,
        spec: &UiPanelSpec,
        node: &bitty_lua::UiRichNode,
    ) -> Result<i64, BridgeError> {
        if self.scene_host.is_none() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        }
        // One layer per generation, checked before the panel so a plugin that
        // re-opens its own panel gets the nesting diagnosis rather than a busy
        // one about itself.
        if self
            .ui_panel_layers
            .values()
            .any(|layer| layer.owner == *owner && layer.generation == generation)
        {
            return Err(BridgeError::new(
                "runtime",
                E_UI_PANEL_NESTED,
                "this plugin generation already presents a panel content layer",
            ));
        }
        let Some(view) = self.focused_view() else {
            return Err(panel_unavailable("no focused panel to present into"));
        };
        let Some(panel) = self.ui_panel_target(view)? else {
            return Err(panel_busy());
        };
        if !matches!(
            self.panel_content_kind(view),
            Some(ViewContent::Terminal(_))
        ) {
            return Err(panel_unavailable(
                "the focused panel is not terminal-backed",
            ));
        }
        let scene = self.ui_panel_scene(panel, owner, generation, node)?;
        let presented = self
            .present_panel_content(view, scene)
            .map_err(|error| match error {
                PanelError::ContentUnavailable { reason, .. } => panel_unavailable(reason),
                PanelError::ContentBusy { .. } => panel_busy(),
                other => BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content presentation refused: {other}"),
                ),
            })?;
        let handle = self.next_ui_panel_handle;
        self.next_ui_panel_handle = self.next_ui_panel_handle.saturating_add(1);
        self.ui_panel_layers.insert(
            handle,
            UiPanelLayerRecord {
                handle,
                owner: owner.clone(),
                generation,
                panel,
                view,
                token: presented.token,
                spec: *spec,
            },
        );
        // The layer owns a Core-side key queue from the moment it exists, so
        // the input gate never has to allocate one on the hot path.
        self.ui_panel_input
            .insert(handle, PanelContentInputQueue::new());
        Ok(handle)
    }

    /// Replaces the content of the layer `handle` names (bitty#2388
    /// `bitty.ui.panel.update`).
    ///
    /// Only the painted scene changes: the panel, its attachment, its
    /// placement, the displaced terminal, and the frozen geometry are exactly
    /// as the present left them, so an update can never be used to move a
    /// panel or to resize a hidden PTY.
    ///
    /// # Errors
    ///
    /// [`E_UI_NOT_OWNER`] for a handle this generation does not hold (stale,
    /// foreign, presented by another generation, already released, or whose
    /// Core record is gone — the entry is dropped, so a repeat answers the same
    /// way); `E_UI_COMPONENT_INVALID` for a refused scene; [`E_UI_UNAVAILABLE`]
    /// when no panel host is installed. A refusal keeps the last good content.
    pub fn ui_panel_update(
        &mut self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        node: &bitty_lua::UiRichNode,
    ) -> Result<bool, BridgeError> {
        let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
            return Err(not_owner());
        };
        if layer.owner != *owner || layer.generation != generation {
            return Err(not_owner());
        }
        let Some(host) = self.scene_host.clone() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        };
        let scene = self.ui_panel_scene(layer.panel, owner, generation, node)?;
        let replaced = match host.try_borrow_mut() {
            Ok(mut host) => host.replace_presented_content_scene(layer.view, layer.token, scene),
            Err(_) => Err(PanelError::ContentUnavailable {
                view_id: layer.view,
                reason: "panel host is borrowed",
            }),
        };
        match replaced {
            Ok(()) => {}
            Err(
                PanelError::ContentNotPresented { .. } | PanelError::ContentTokenMismatch { .. },
            ) => {
                // The record left between the lookup and the write: the layer
                // was released (owner path, generation edge, or disposal), so
                // the handle names nothing and must not come back to life.
                self.ui_panel_layers.remove(&handle);
                return Err(not_owner());
            }
            Err(PanelError::ContentUnavailable { reason, .. }) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content update refused: {reason}"),
                ));
            }
            Err(other) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    format!("panel content update refused: {other}"),
                ));
            }
        }
        // The layer's painted content moved: drop the retained leaf primitives
        // so the next frame is a full paint of the new scene rather than a diff
        // against the old one.
        self.presented_leaf_frames.remove(&layer.view);
        self.pending_full_redraw = true;
        Ok(true)
    }

    /// Releases the layer `handle` names and restores the terminal it displaced
    /// (bitty#2388 `bitty.ui.panel.release`).
    ///
    /// The restore is the Core token release, so it keeps every guarantee that
    /// path already has: the recorded content kind, the same panel and
    /// attachment, the prior presentation mode, and exactly one resize when the
    /// leaf's content rect moved while the layer was up.
    ///
    /// `reason` is the caller's validated disposition. It is recorded on the
    /// `panel.content.released` observation and on the remembered release a
    /// later `poll` reports; it never changes what is restored. Core-reported
    /// causes overwrite it on the paths Core owns (see
    /// [`PANEL_CONTENT_RELEASE_CLOSED`] and
    /// [`PANEL_CONTENT_RELEASE_UNLOADED`]).
    ///
    /// # Errors
    ///
    /// [`E_UI_NOT_OWNER`] for a handle this generation does not hold, including
    /// one a previous generation presented, and a second release of the same
    /// handle (the record is gone and the entry is dropped);
    /// [`E_UI_PANEL_UNAVAILABLE`] when the panel is no longer mounted;
    /// [`E_UI_UNAVAILABLE`] when no panel host is installed. A refusal restores
    /// nothing and leaves a live layer presented.
    pub fn ui_panel_release(
        &mut self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        reason: &str,
    ) -> Result<bool, BridgeError> {
        let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
            return Err(not_owner());
        };
        if layer.owner != *owner || layer.generation != generation {
            return Err(not_owner());
        }
        match self.release_panel_content_at(layer.view, layer.token, reason) {
            // `release_panel_content_at` prunes the row and records the release
            // observation itself (one prune point for every release path), so
            // nothing else to drop here.
            Ok(_) => Ok(true),
            Err(
                PanelError::ContentNotPresented { .. } | PanelError::ContentTokenMismatch { .. },
            ) => {
                self.ui_panel_layers.remove(&handle);
                self.ui_panel_input.remove(&handle);
                Err(not_owner())
            }
            Err(PanelError::ContentUnavailable { reason, .. }) => {
                self.ui_panel_layers.remove(&handle);
                self.ui_panel_input.remove(&handle);
                Err(panel_unavailable(reason))
            }
            Err(other) => Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                format!("panel content release refused: {other}"),
            )),
        }
    }

    /// Drains the Core-owned key queue of the layer `handle` names
    /// (bitty#2388 `bitty.ui.panel.poll`).
    ///
    /// This is the only plugin-visible input path for a presented panel. It is
    /// an owner-initiated cold-path read: Core never calls a plugin callback on
    /// the input path (`P0-AC-015`), so the hot path stays one queue append.
    /// Events come back oldest-first, `max` is clamped to
    /// [`UI_PANEL_INPUT_QUEUE_MAX`], and the sticky `overflowed` flag reports
    /// whether the drop-oldest bound discarded anything.
    ///
    /// After Core ended the layer, the owner's next poll reports `released`
    /// with the recorded reason and no events — the same remembered-release
    /// shape the focusable overlay capture offers. Draining does not end the
    /// layer and changes no content, geometry, or focus.
    ///
    /// # Errors
    ///
    /// [`E_UI_NOT_OWNER`] for a handle this generation never held, one another
    /// generation holds, or one that was released and is no longer remembered.
    /// A refusal drains nothing.
    pub fn ui_panel_poll(
        &mut self,
        owner: &PluginId,
        generation: u32,
        handle: i64,
        max: usize,
    ) -> Result<PanelPoll, BridgeError> {
        let max = max.clamp(1, UI_PANEL_INPUT_QUEUE_MAX);
        if let Some(layer) = self.ui_panel_layers.get(&handle) {
            if layer.owner != *owner || layer.generation != generation {
                return Err(not_owner());
            }
            let Some(queue) = self.ui_panel_input.get_mut(&handle) else {
                // A live row without a queue is a host bug, not a plugin one:
                // refuse instead of inventing a queue the plugin never drained.
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    "panel content key queue is missing for a live layer",
                ));
            };
            let events = queue.drain(max);
            return Ok(PanelPoll {
                active: true,
                seq: queue.last_delivered(),
                events,
                overflowed: queue.overflowed(),
                reason: None,
            });
        }
        if let Some(record) = self.ui_panel_released.get(&handle) {
            if record.owner == *owner && record.generation == generation {
                return Ok(PanelPoll {
                    active: false,
                    seq: record.seq,
                    events: Vec::new(),
                    overflowed: record.overflowed,
                    reason: Some(record.reason.clone()),
                });
            }
        }
        Err(not_owner())
    }

    /// Number of queued (undrained) key events for the layer `handle`.
    ///
    /// Read-only projection for diagnostics and tests: it carries no key text
    /// and no terminal content.
    #[must_use]
    pub fn ui_panel_queued(&self, handle: i64) -> usize {
        self.ui_panel_input
            .get(&handle)
            .map_or(0, PanelContentInputQueue::len)
    }

    /// The remembered release of `handle`, if a later `poll` may still report
    /// it (bitty#2388 `bitty.ui.panel.poll`).
    #[must_use]
    pub fn ui_panel_release_record(&self, handle: i64) -> Option<&UiPanelReleaseRecord> {
        self.ui_panel_released.get(&handle)
    }

    /// Drains the bounded `panel.content.*` observations the runtime produced
    /// (bitty#2388 D8, CTX-1911).
    ///
    /// Two sources, both bounded: the release observations queued by every
    /// release path ([`PANEL_CONTENT_EVENT_QUEUE_MAX`]), and one
    /// level-triggered input-availability nudge per live layer whose queue is
    /// still non-empty. Deriving the nudge from the queue instead of counting
    /// keys keeps the input path free of observation bookkeeping, and makes it
    /// "at most one per layer per drain while the queue stays non-empty" by
    /// construction.
    ///
    /// The caller turns each observation into the closed bus kind named by
    /// [`PanelContentEvent::kind`]
    /// ([`PluginRuntime::pump_panel_content_events`](crate::plugin_runtime::PluginRuntime::pump_panel_content_events)
    /// does it in one call).
    pub fn drain_panel_content_events(&mut self) -> Vec<PanelContentEvent> {
        let mut events: Vec<PanelContentEvent> = self.ui_panel_events.drain(..).collect();
        let pending: Vec<(i64, String, usize)> = self
            .ui_panel_layers
            .iter()
            .filter_map(|(handle, layer)| {
                let queue = self.ui_panel_input.get(handle)?;
                if queue.is_empty() {
                    return None;
                }
                Some((*handle, layer.owner.as_str().to_string(), queue.len()))
            })
            .collect();
        events.extend(pending.into_iter().map(|(handle, owner, queued)| {
            PanelContentEvent::InputAvailable {
                owner,
                handle,
                queued,
            }
        }));
        events
    }

    /// Appends one key event to the focused presented layer's queue.
    ///
    /// Runs on the Core input path and never invokes plugin code: it resolves
    /// the focused view's layer, appends one bounded event, and returns. A
    /// presented layer that no plugin row owns (a Core-presented layer) has no
    /// queue, so the key is swallowed and counted exactly as before — never
    /// forwarded to the hidden shell.
    pub(super) fn capture_panel_content_key(
        &mut self,
        event: &bitty_platform::KeyEvent,
        is_modifier: bool,
    ) {
        if is_modifier || event.is_synthetic || event.state != bitty_platform::PressState::Pressed {
            return;
        }
        let Some(view) = self.focused_view() else {
            return;
        };
        let Some(handle) = self
            .ui_panel_layers
            .values()
            .find(|layer| layer.view == view)
            .map(|layer| layer.handle)
        else {
            return;
        };
        let text = panel_content_key_text(event);
        let chord = self.panel_content_key_chord(&text);
        if let Some(queue) = self.ui_panel_input.get_mut(&handle) {
            queue.push_key(&text, chord);
        }
    }

    /// Chord identity of one key routed to a presented panel.
    ///
    /// Reports the modifier bits the runtime is tracking, using the same
    /// bounded spelling as the event's `text` field so a plugin can match the
    /// chord it would bind. No modifiers means no chord, byte-identical to an
    /// unmodified key.
    fn panel_content_key_chord(&self, text: &str) -> Option<OverlayKeyChord> {
        let (ctrl, alt, shift, super_held) = (
            self.control_pressed,
            self.alt_pressed,
            self.shift_pressed,
            self.super_pressed(),
        );
        if !(ctrl || alt || shift || super_held) {
            return None;
        }
        Some(OverlayKeyChord::new(text, ctrl, alt, shift, super_held))
    }

    /// Records one ended plugin layer: drops its queue, queues the bounded
    /// `panel.content.released` observation, and remembers the release for the
    /// owner's next `poll`.
    ///
    /// One call site shape for every release path, so an event can never be
    /// claimed for a layer that was not actually released.
    pub(super) fn note_panel_content_release(&mut self, layer: &UiPanelLayerRecord, reason: &str) {
        let (overflowed, seq) = self
            .ui_panel_input
            .remove(&layer.handle)
            .map_or((false, 0), |queue| {
                (queue.overflowed(), queue.last_delivered())
            });
        let reason = bounded_reason(reason);
        self.push_panel_content_event(PanelContentEvent::Released {
            owner: layer.owner.as_str().to_string(),
            handle: layer.handle,
            reason: reason.clone(),
        });
        self.ui_panel_released.insert(
            layer.handle,
            UiPanelReleaseRecord {
                owner: layer.owner.clone(),
                generation: layer.generation,
                reason,
                overflowed,
                seq,
            },
        );
        while self.ui_panel_released.len() > PANEL_CONTENT_RELEASE_MEMORY_MAX {
            let Some(oldest) = self.ui_panel_released.keys().next().copied() else {
                break;
            };
            self.ui_panel_released.remove(&oldest);
        }
    }

    /// Appends one observation, dropping the oldest at the queue bound.
    fn push_panel_content_event(&mut self, event: PanelContentEvent) {
        if self.ui_panel_events.len() >= PANEL_CONTENT_EVENT_QUEUE_MAX {
            self.ui_panel_events.pop_front();
        }
        self.ui_panel_events.push_back(event);
    }

    /// Releases every layer `generation` of `owner` presented (bitty#2388
    /// "Crash/disable/reload/suspend: teardown-ordered release").
    ///
    /// Called by the generation edge before the placeholder is presented, so a
    /// dead generation's layer never outlives it and the panel comes back to
    /// its terminal instead of staying covered by content nobody owns. The edge
    /// hands over the lifecycle cause (`suspended`, `reloaded`, `disabled`,
    /// `unloaded`, `crashed`), which is recorded on the layer's
    /// `panel.content.released` observation. A layer the Core generation edge
    /// already released (it releases by binding, not by token) is simply
    /// forgotten: the entry is dropped and no second restore runs. Returns how
    /// many layers this call released.
    pub fn ui_panel_release_owner(
        &mut self,
        owner: &PluginId,
        generation: u32,
        reason: &str,
    ) -> usize {
        let handles: Vec<i64> = self
            .ui_panel_layers
            .values()
            .filter(|layer| layer.owner == *owner && layer.generation == generation)
            .map(|layer| layer.handle)
            .collect();
        let mut released = 0;
        for handle in handles {
            let Some(layer) = self.ui_panel_layers.get(&handle).cloned() else {
                continue;
            };
            match self.release_panel_content_at(layer.view, layer.token, reason) {
                Ok(_) => released += 1,
                // Already gone (the Core generation edge owns that path) or the
                // panel is disposed: either way the row must not survive.
                Err(_) => {
                    self.ui_panel_layers.remove(&handle);
                    self.ui_panel_input.remove(&handle);
                }
            }
        }
        released
    }

    /// The layer `handle` names, if this runtime still holds it.
    ///
    /// Read-only projection for diagnostics and tests: it carries the owner,
    /// the generation, the attachment, and the recorded descriptor, and it
    /// exposes no terminal content, no PTY, and no key bytes.
    #[must_use]
    pub fn ui_panel_layer(&self, handle: i64) -> Option<&UiPanelLayerRecord> {
        self.ui_panel_layers.get(&handle)
    }

    /// Number of plugin-presented layers this runtime holds.
    #[must_use]
    pub fn ui_panel_layer_count(&self) -> usize {
        self.ui_panel_layers.len()
    }

    /// Drops the plugin row of a released Core layer.
    ///
    /// Every Core release path funnels here through
    /// [`Runtime::release_panel_content`], so a handle can never outlive the
    /// record it names (see the module's "Handle lifetime"). The layer's key
    /// queue goes with it: a released handle must not answer `poll` with keys
    /// nobody will ever read.
    pub(super) fn forget_ui_panel_layer(&mut self, token: PanelContentToken) {
        let handles: Vec<i64> = self
            .ui_panel_layers
            .iter()
            .filter(|(_, layer)| layer.token == token)
            .map(|(handle, _)| *handle)
            .collect();
        for handle in handles {
            self.ui_panel_layers.remove(&handle);
            self.ui_panel_input.remove(&handle);
        }
    }

    /// Resolves the panel the present would attach to, refusing a panel that
    /// already presents a layer.
    ///
    /// Returns `Ok(None)` when the panel already presents a content layer —
    /// whoever owns it; the one-layer-per-generation rule was checked first, so
    /// a `None` here is always another owner's layer — and the caller reports
    /// `E_UI_PANEL_BUSY` without a second host borrow.
    fn ui_panel_target(&self, view: ViewId) -> Result<Option<PanelId>, BridgeError> {
        let Some(host) = self.scene_host.as_ref() else {
            return Err(BridgeError::new(
                "runtime",
                E_UI_UNAVAILABLE,
                "host has no panel content surface",
            ));
        };
        let host = match host.try_borrow() {
            Ok(host) => host,
            Err(_) => {
                return Err(BridgeError::new(
                    "runtime",
                    E_UI_UNAVAILABLE,
                    "panel host is borrowed",
                ));
            }
        };
        if host.presented_content_for_view(view).is_some() {
            return Ok(None);
        }
        match host.content_panel_for_view(view) {
            Ok(panel) => Ok(Some(panel)),
            Err(_) => Err(panel_unavailable("no panel is mounted at the focused view")),
        }
    }

    /// Builds one admitted scene for a layer from the bridge's validated tree.
    ///
    /// The block id is the panel's: a layer replaces that panel's content, so a
    /// stable id per container keeps the paint slot addressable across updates.
    /// An admission refusal already carries `E_UI_COMPONENT_INVALID` — that is
    /// the value of [`crate::scene_host::E_SCENE_REFUSED`], the record's code for
    /// a scene the host will not paint — while a missing admission path stays
    /// [`E_UI_UNAVAILABLE`]. Nothing is re-spelled here.
    fn ui_panel_scene(
        &self,
        panel: PanelId,
        owner: &PluginId,
        generation: u32,
        node: &bitty_lua::UiRichNode,
    ) -> Result<Scene, BridgeError> {
        crate::scene_host::build_panel_scene(BlockId(panel.get()), owner.as_str(), generation, node)
    }
}

/// `E_UI_PANEL_BUSY`: the panel already presents somebody's layer.
fn panel_busy() -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_PANEL_BUSY,
        "the focused panel already presents a content layer",
    )
}

/// `E_UI_PANEL_UNAVAILABLE`: no usable target panel.
fn panel_unavailable(reason: impl Into<String>) -> BridgeError {
    BridgeError::new("runtime", E_UI_PANEL_UNAVAILABLE, reason)
}

/// `E_UI_NOT_OWNER`: the handle is not this generation's live layer.
fn not_owner() -> BridgeError {
    BridgeError::new(
        "runtime",
        E_UI_NOT_OWNER,
        "handle is not a live panel content layer of this plugin generation",
    )
}

/// Bounded text of one key event routed to a presented panel.
///
/// The record admits content keys (`Esc`, `q`, `j`, `k`, …) — the keys the
/// shell would otherwise receive — so the payload is the produced text when
/// the platform reports one (`j`, `k`, space) and the key's own name otherwise
/// (`Escape`, `Enter`, `ArrowUp`), because a control key produces no text and
/// a bare "nothing" would make `Esc` indistinguishable from an unknown key.
/// The spelling is the platform `NamedKey` name, which is this candidate
/// surface's own vocabulary: it is not the keymap chord spelling the overlay
/// capture uses, and no plugin binds through the keymap here.
fn panel_content_key_text(event: &bitty_platform::KeyEvent) -> String {
    if let Some(text) = event.text.as_deref() {
        if !text.is_empty() && !text.chars().all(char::is_control) {
            return bounded_panel_text(text);
        }
    }
    match &event.logical_key {
        bitty_platform::LogicalKey::Character(chars) => bounded_panel_text(chars),
        bitty_platform::LogicalKey::Named(named) => format!("{named:?}"),
        _ => "unidentified".to_string(),
    }
}
