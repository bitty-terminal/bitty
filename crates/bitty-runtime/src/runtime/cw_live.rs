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
//!   and [`crate::cw_present::fold_present`];
//! - issue #982 (CW-03): [`Runtime::cw_input_route`] +
//!   [`Runtime::cw_composer_feed`] + [`Runtime::cw_composer_snapshot`] call
//!   [`crate::cw_present::route_present_input`],
//!   [`crate::cw_present::feed_present`], and
//!   [`crate::cw_present::composer_present`];
//! - issue #983 (CW-04): [`Runtime::cw_hint_register`] +
//!   [`Runtime::cw_hint_collect`] + [`Runtime::cw_hint_dispatch`] own one
//!   [`crate::cw_present::CwHintEngine`] for labels, overlay, and dispatch;
//! - all seven slices: [`Runtime::cw_present_plan`] calls
//!   [`crate::cw_present::plan_present`] once per present derivation.
//!
//! Headless, bounded, presentation-only: the grid, scrollback, PTY, GPU,
//! and filesystem are never touched here. The composer editor stays a
//! routing flag (no process is spawned); grid truth stays with `State`.

use super::*;

use bitty_rich::blocks::{CommandBlock, CommandId};
use bitty_rich::composer::{ComposerFeedError, ComposerKeyEvent};
use bitty_rich::hints::{DispatchError, DispatchOutcome, HintAction, HintBatch, HintScope};
use bitty_rich::scene::Scene;

use crate::cw_present::{
    ComposerPresent, CwComposerFeed, CwFoldAction, CwHintProvider, CwInputRoute, CwPresentInputs,
    CwPresentPlan, FoldPresent, apply_fold_action, composer_present, dispatch_present,
    feed_present, fold_present, plan_present, route_present_input,
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

    /// Number of providers registered on the live hint engine.
    #[must_use]
    pub fn cw_hint_provider_count(&self) -> usize {
        self.cw_hints.provider_count()
    }

    /// Collects one cross-panel hint batch against live terminal state.
    ///
    /// One engine owns provider registration, label allocation, the overlay
    /// batch, and dispatch, so labels stay unique across panels and the
    /// overlay stays one layer with zero overlay-slot cost.
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
