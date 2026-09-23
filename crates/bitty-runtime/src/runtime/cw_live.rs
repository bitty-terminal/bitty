#![forbid(unsafe_code)]
//! Live CW present wiring (CTX-0700).
//!
//! [`Runtime`] is the live owner of the three headless CW present states:
//! the caller-owned fold projection, the composer overlay session, and the
//! single cross-panel hint engine. Every method below is a live-path
//! consumer of the evidence-only [`crate::cw_present`] candidates:
//!
//! - issue #980 (CW-01): [`Runtime::cw_fold_apply`] +
//!   [`Runtime::cw_fold_projection`] call [`crate::cw_present::apply_fold_action`]
//!   and [`crate::cw_present::fold_present`]; [`Runtime::cw_fold_latest`]
//!   applies a verb to the focused view's latest command block for the
//!   `fold_toggle`/`fold_expand`/`fold_collapse` keymap actions;
//! - issue #981 (CW-02): the Leader-armed [`Runtime::cw_hint_arm`] +
//!   [`Runtime::cw_hint_push_key`] + [`Runtime::cw_hint_disarm`] own the
//!   live [`bitty_rich::hints::HintSession`] behind the operator-conflict
//!   gate with prefix-completion dispatch;
//! - issue #982 (CW-03): [`Runtime::cw_input_route`] +
//!   [`Runtime::cw_composer_feed`] + [`Runtime::cw_composer_snapshot`] call
//!   [`crate::cw_present::route_present_input`],
//!   [`crate::cw_present::feed_present`], and
//!   [`crate::cw_present::composer_present`];
//! - issue #983 (CW-04): [`Runtime::cw_hint_register`] +
//!   [`Runtime::cw_hint_unregister`] + [`Runtime::cw_hint_collect`] +
//!   [`Runtime::cw_hint_dispatch`] own one
//!   [`crate::cw_present::CwHintEngine`] for labels, overlay, and dispatch;
//!   [`Runtime::cw_hint_dispatch_armed`] is the session-authority live path
//!   (issue #981): it dispatches against the armed batch only, never a
//!   caller-supplied one;
//! - issue #984 (CW-05): [`Runtime::cw_fold_snapshot_ordinals`] +
//!   [`Runtime::cw_fold_restore_ordinals`] own fold persistence as anchor
//!   ordinals (stable scrollback identity stays deferred per owner ruling);
//! - all seven slices: [`Runtime::cw_present_plan`] calls
//!   [`crate::cw_present::plan_present`] once per present derivation.
//!
//! Headless, bounded, presentation-only: the grid, scrollback, PTY, GPU,
//! and filesystem are never touched here. The composer editor stays a
//! routing flag (no process is spawned); grid truth stays with `State`.

use super::*;

use bitty_rich::blocks::{CommandBlock, CommandId, blocks};
use bitty_rich::composer::{ComposerFeedError, ComposerKeyEvent};
use bitty_rich::hints::{
    DispatchError, DispatchOutcome, HINT_LABEL_MAX_CHARS, HintAction, HintBatch, HintFeedError,
    HintOperator, HintScope, OperatorConflict,
};
use bitty_rich::scene::Scene;

use crate::cw_present::{
    ComposerPresent, CwComposerFeed, CwFoldAction, CwHintProvider, CwInputRoute, CwPresentInputs,
    CwPresentPlan, FoldPresent, HintKeyOutcome, apply_fold_action, composer_present,
    dispatch_present, feed_present, fold_present, persist_fold_ordinals, plan_present,
    route_present_input,
};

impl Runtime {
    /// Applies one present-path fold verb to the live fold state (issue #980).
    ///
    /// Returns `true` when the requested end state holds afterwards; see
    /// [`apply_fold_action`] for the per-verb contract. Terminal truth is
    /// untouched.
    pub fn cw_fold_apply(&mut self, id: CommandId, action: CwFoldAction) -> bool {
        apply_fold_action(&mut self.cw_fold, id, action)
    }

    /// Whether `id` is currently folded in the live present state.
    #[must_use]
    pub fn cw_fold_is_folded(&self, id: CommandId) -> bool {
        self.cw_fold.is_folded(id)
    }

    /// Derives the live fold overlay projection for `blocks` (issue #980).
    ///
    /// Splits the frame's command blocks into visible/hidden id lists under
    /// the live [`bitty_rich::blocks::FoldState`]. Hidden rows are a paint
    /// concern only.
    #[must_use]
    pub fn cw_fold_projection(&self, blocks: &[CommandBlock]) -> FoldPresent {
        fold_present(blocks, &self.cw_fold)
    }

    /// Explicitly opens the composer overlay (issue #982).
    ///
    /// Opening is always explicit; there is no auto-enter path.
    pub fn cw_composer_open(&mut self) {
        self.cw_composer.open();
    }

    /// Closes the composer overlay, preserving the draft for reopen.
    pub fn cw_composer_close(&mut self) {
        self.cw_composer.close();
    }

    /// Whether the composer overlay paints this frame.
    #[must_use]
    pub fn cw_composer_is_open(&self) -> bool {
        self.cw_composer.is_open()
    }

    /// Current composer draft content.
    #[must_use]
    pub fn cw_composer_content(&self) -> &str {
        self.cw_composer.content()
    }

    /// Routes input for the current present frame around the composer
    /// session (issue #982).
    ///
    /// Closed routes to the PTY byte-identically; open routes to the
    /// composer via [`Runtime::cw_composer_feed`].
    #[must_use]
    pub fn cw_input_route(&self) -> CwInputRoute {
        route_present_input(&self.cw_composer)
    }

    /// Feeds one key press through the live present-path composer.
    ///
    /// Closed sessions return [`CwComposerFeed::PtyPassthrough`] (normal-mode
    /// input stays PTY byte-identical). The external-editor outcome is a
    /// routing flag only; no process is spawned here.
    pub fn cw_composer_feed(&mut self, ev: ComposerKeyEvent) -> CwComposerFeed {
        feed_present(&mut self.cw_composer, ev)
    }

    /// Snapshots the live composer overlay state for one present frame.
    #[must_use]
    pub fn cw_composer_snapshot(&self) -> ComposerPresent {
        composer_present(&self.cw_composer)
    }

    /// Applies an external-editor round-trip result back to the live draft.
    ///
    /// Fails closed past the composer cap with the old content kept.
    ///
    /// # Errors
    ///
    /// [`ComposerFeedError::TooLarge`] when `content` exceeds the cap.
    pub fn cw_composer_apply_external(&mut self, content: &str) -> Result<(), ComposerFeedError> {
        self.cw_composer.apply_external_result(content)
    }

    /// Registers one panel provider on the live cross-panel hint engine
    /// (issue #983).
    ///
    /// Returns `false` (fail-closed, engine untouched) past the provider cap
    /// or for a duplicate panel.
    pub fn cw_hint_register(&mut self, provider: CwHintProvider) -> bool {
        self.cw_hints.register_provider(provider)
    }

    /// Unregisters one panel provider on the live hint engine (issue #983).
    ///
    /// Dispose symmetry for [`Runtime::cw_hint_register`]: a disposed panel
    /// stops contributing targets so its labels cannot outlive the leaf.
    /// Returns `true` when a provider was removed.
    pub fn cw_hint_unregister(&mut self, panel: u64) -> bool {
        self.cw_hints.unregister_provider(panel)
    }

    /// Number of providers registered on the live hint engine.
    #[must_use]
    pub fn cw_hint_provider_count(&self) -> usize {
        self.cw_hints.provider_count()
    }

    /// Collects one cross-panel hint batch against live terminal state.
    ///
    /// One engine owns provider registration, label allocation, the overlay
    /// batch, and dispatch, so labels stay unique across panels and the
    /// overlay stays one layer with zero overlay-slot cost. Command-block
    /// targets join every collection (the live engine enables them).
    #[must_use]
    pub fn cw_hint_collect(&self, scope: HintScope, generation: u64) -> HintBatch {
        self.cw_hints.collect(&self.state, scope, generation)
    }

    /// Dispatches `Action(Target)` from the live present path.
    ///
    /// Resolves `label` in `batch` and applies `action`, mutating only the
    /// live fold state.
    ///
    /// # Errors
    ///
    /// [`DispatchError::UnknownLabel`] or the action-specific rejection from
    /// [`dispatch_present`].
    pub fn cw_hint_dispatch(
        &mut self,
        batch: &HintBatch,
        label: &str,
        action: HintAction,
    ) -> Result<DispatchOutcome, DispatchError> {
        dispatch_present(batch, &mut self.cw_fold, label, action)
    }

    /// Dispatches `Action(Target)` against the armed session's live batch
    /// (issue #981 dispatch authority, CTX-0735).
    ///
    /// The armed batch is the single dispatch authority: unlike
    /// [`Runtime::cw_hint_dispatch`] (which trusts a caller-supplied batch),
    /// this takes no batch at all, so a stale or foreign batch can never be
    /// smuggled through the live path. Success disarms the session (hint
    /// chrome is ephemeral); dispatch errors keep the session armed so the
    /// caller can retry the label inside the same window.
    ///
    /// # Errors
    ///
    /// [`HintFeedError::NotArmed`] while disarmed (or armed with no batch:
    /// keys belong to the shell then); [`HintFeedError::Dispatch`] for the
    /// [`dispatch_present`] rejection, with the fold untouched.
    pub fn cw_hint_dispatch_armed(
        &mut self,
        label: &str,
        action: HintAction,
    ) -> Result<DispatchOutcome, HintFeedError> {
        if !self.cw_hint_session.is_armed() {
            return Err(HintFeedError::NotArmed);
        }
        let outcome = match self.cw_hint_session.batch() {
            Some(batch) => dispatch_present(batch, &mut self.cw_fold, label, action)?,
            None => return Err(HintFeedError::NotArmed),
        };
        self.cw_hint_disarm();
        Ok(outcome)
    }

    /// Arms the live hint session from the live engine (issue #981).
    ///
    /// Collects one batch against live terminal state through the single
    /// [`CwHintEngine`](crate::cw_present::CwHintEngine) (labels stay unique
    /// across panels) and arms the session behind the operator-conflict
    /// gate. Returns the armed label count so the caller can refuse an
    /// empty batch loudly instead of arming a dead session.
    ///
    /// # Errors
    ///
    /// [`OperatorConflict`] when an operator key collides with
    /// `bound_bare_keys`; the session stays disarmed and keeps no batch.
    pub fn cw_hint_arm(
        &mut self,
        scope: HintScope,
        generation: u64,
        bound_bare_keys: &[char],
    ) -> Result<usize, OperatorConflict> {
        let batch = self.cw_hints.collect(&self.state, scope, generation);
        let count = batch.len();
        self.cw_hint_session.arm(batch, bound_bare_keys)?;
        self.cw_hint_operator = None;
        self.cw_hint_label.clear();
        Ok(count)
    }

    /// Whether the live hint session currently owns operator/label keys.
    #[must_use]
    pub fn cw_hint_is_armed(&self) -> bool {
        self.cw_hint_session.is_armed()
    }

    /// Disarms the live hint session, dropping the batch and any partial
    /// operator/label keystrokes (issue #981).
    ///
    /// Hint chrome is ephemeral: disarm is always safe, idempotent, and
    /// touches neither terminal truth nor the fold state.
    pub fn cw_hint_disarm(&mut self) {
        self.cw_hint_session.disarm();
        self.cw_hint_operator = None;
        self.cw_hint_label.clear();
    }

    /// Feeds one keystroke into the armed hint interaction (issue #981).
    ///
    /// The first letter selects the operator (`j`/`z`/`p`/`y`/`e`/`c`); the
    /// following letters accumulate the label and dispatch once the buffer
    /// resolves exactly with no longer label extending it (prefix
    /// completion). A dead label buffer (resolves nothing and extends
    /// nothing) is cleared so the caller can retry the label inside the
    /// same armed window; the operator is kept. Dispatch disarms the
    /// session. Fails closed with [`HintKeyOutcome::Invalid`] while
    /// disarmed — keys belong to the shell then, never to this method.
    pub fn cw_hint_push_key(&mut self, raw: char) -> HintKeyOutcome {
        if !self.cw_hint_session.is_armed() {
            return HintKeyOutcome::Invalid { key: raw };
        }
        let key = raw.to_ascii_lowercase();
        if !key.is_ascii_lowercase() {
            return HintKeyOutcome::Invalid { key: raw };
        }
        if self.cw_hint_operator.is_none() {
            if HintOperator::from_key(key).is_some() {
                self.cw_hint_operator = Some(key);
                return HintKeyOutcome::NeedMore;
            }
            return HintKeyOutcome::Invalid { key: raw };
        }
        if self.cw_hint_label.len() >= HINT_LABEL_MAX_CHARS {
            return HintKeyOutcome::Invalid { key: raw };
        }
        self.cw_hint_label.push(key);
        let (resolves, extendable) = match self.cw_hint_session.batch() {
            Some(batch) => {
                let resolves = batch.resolve(&self.cw_hint_label).is_some();
                let extendable = batch.labels().iter().any(|l| {
                    l.label.len() > self.cw_hint_label.len()
                        && l.label.starts_with(self.cw_hint_label.as_str())
                });
                (resolves, extendable)
            }
            None => (false, false),
        };
        if resolves && !extendable {
            let operator = self.cw_hint_operator.unwrap_or(key);
            let label = self.cw_hint_label.clone();
            match self
                .cw_hint_session
                .feed(&mut self.cw_fold, operator, label.as_str())
            {
                Ok(outcome) => {
                    self.cw_hint_disarm();
                    HintKeyOutcome::Dispatched(outcome)
                }
                Err(_) => HintKeyOutcome::Invalid { key: raw },
            }
        } else if !resolves && !extendable {
            self.cw_hint_label.clear();
            HintKeyOutcome::Invalid { key: raw }
        } else {
            HintKeyOutcome::NeedMore
        }
    }

    /// Latest semantic command block on the focused view's state, if shell
    /// integration has marked any (issue #980).
    ///
    /// `None` with no `OSC 133` zones yet: the fold verbs below stay
    /// fail-closed instead of inventing a target.
    #[must_use]
    pub fn cw_latest_command_id(&self) -> Option<CommandId> {
        blocks(&self.state).last().map(|block| block.id)
    }

    /// Applies one present-path fold verb to the focused view's latest
    /// command block (issue #980).
    ///
    /// Returns `None` when no command block exists yet; otherwise the
    /// [`apply_fold_action`](crate::cw_present::apply_fold_action) end-state
    /// contract. Terminal truth is untouched.
    pub fn cw_fold_latest(&mut self, action: CwFoldAction) -> Option<bool> {
        let id = self.cw_latest_command_id()?;
        Some(apply_fold_action(&mut self.cw_fold, id, action))
    }

    /// Serializes live fold membership as anchor ordinals (issue #984).
    ///
    /// The present layer owns persistence: ordinals (never grid rows) leave
    /// here and replay through [`Runtime::cw_fold_restore_ordinals`] on
    /// rehydrate. Bounded by `FOLD_MAX` by construction.
    #[must_use]
    pub fn cw_fold_snapshot_ordinals(&self) -> Vec<u64> {
        persist_fold_ordinals(&self.cw_fold)
    }

    /// Replays persisted fold ordinals into the live fold state, returning
    /// the admitted count (issue #984).
    ///
    /// Each ordinal replays idempotently through
    /// [`apply_fold_action`](crate::cw_present::apply_fold_action)
    /// (`Collapse`); ordinals past the fold cap fail closed and are not
    /// counted. Unknown ordinals (evicted history) are admitted harmlessly:
    /// they contribute nothing to any projection until their blocks exist
    /// again.
    pub fn cw_fold_restore_ordinals(&mut self, ordinals: &[u64]) -> usize {
        let mut admitted = 0usize;
        for raw in ordinals {
            // `Collapse` is idempotent: already-folded holds trivially and a
            // fresh fold reports its insertion, so one call covers both arms
            // (fail-closed past the cap reports `false` and is not counted).
            if apply_fold_action(&mut self.cw_fold, CommandId(*raw), CwFoldAction::Collapse)
                || self.cw_fold.is_folded(CommandId(*raw))
            {
                admitted += 1;
            }
        }
        admitted
    }

    /// Derives the one-frame CW present enrichment for a live view
    /// (issues #980/#982/#983 and the remaining present slices).
    ///
    /// Composes the fold projection, semantic anchors, hint batch summary,
    /// composer overlay + routing, rich-scene paint budget, and the
    /// non-terminal payload from headless inputs plus the live fold and
    /// composer states. Pure except for reading live present state; the live
    /// fold is never mutated here (dispatch goes through
    /// [`Runtime::cw_hint_dispatch`]).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn cw_present_plan(
        &self,
        view: ViewId,
        generation: u64,
        blocks: &[CommandBlock],
        hints: &HintBatch,
        scene: &Scene,
        content: bitty_ui::panel::ViewContent,
        node: bitty_ui::uitree::UiNodeId,
    ) -> CwPresentPlan {
        let inputs = CwPresentInputs {
            view,
            generation,
            blocks,
            fold: &self.cw_fold,
            hints,
            composer: &self.cw_composer,
            scene,
            content,
            node,
        };
        plan_present(&inputs)
    }
}
