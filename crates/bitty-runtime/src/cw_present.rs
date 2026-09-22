//! CW render-path present enrichment (CTX-0687, candidate, evidence-only).
//!
//! One headless present-path composer over the seven open CW render-path
//! issues. It consumes the already-accepted headless models — fold, hints,
//! composer, anchors, rich scene, non-terminal panel content — into a single
//! bounded [`CwPresentPlan`] derived per [`PresentFrame`](crate::runtime::PresentFrame)
//! at refresh, without touching grid truth, GPU, PTY, or the filesystem.
//!
//! # Per-issue mapping (all `Relates`, none `Closes`)
//!
//! | Issue | Slice | Entry point |
//! |---|---|---|
//! | #980 CW-01 | fold toggle/expand/collapse into present | [`CwFoldAction`] + [`apply_fold_action`] + [`fold_present`] |
//! | #981 CW-02 | hint overlay + dispatch | [`present_hint_batch`] + [`present_overlay_cost`] + [`dispatch_present`] |
//! | #982 CW-03 | composer overlay + input routing + editor flag | [`CwInputRoute`] + [`feed_present`] + [`composer_present`] |
//! | #983 CW-04 | cross-panel hint API, one engine | [`CwHintEngine`] + [`CwHintProvider`] |
//! | #984 CW-05 | semantic anchor identity + fold persistence | [`SemanticAnchor`] + [`anchor_for_command`] + [`persist_fold_ordinals`] |
//! | #985 CW-06 | consume rich scene in present | [`consume_scene_present`] + [`ScenePresent`] |
//! | #990 CW-11 | non-terminal content beyond the grid | [`NonTerminalPresent`] + [`nonterminal_for_content`] |
//!
//! # Candidate status (read before relying)
//!
//! The owner decisions behind these issues are still open (`OQ-050`
//! scrollback identity, `OQ-051` panel-is-not-terminal, `OQ-088`/`OQ-089`
//! hint leadership). Nothing here resolves them: this module is a
//! presentation-only integration sketch over the existing bounded models,
//! and every verdict in the CTX-0687 PR is `EVIDENCE_ONLY` (`Relates`, never
//! `Closes`). No keymap binding, no overlay allocation, no editor spawn, and
//! no grid write is added.
//!
//! # Terminal truth
//!
//! The only mutation in the system is the caller's [`FoldState`](bitty_rich::blocks::FoldState)
//! via [`apply_fold_action`] / [`dispatch_present`]. Grid, scrollback,
//! zones, clipboard, and snapshots are never written. The composer editor is
//! represented as a routing flag ([`CwComposerFeed::EditorRequested`]); no
//! process is spawned here.
//!
//! # Bounds
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`CwHintEngine`] providers | [`CW_PRESENT_MAX_PROVIDERS`] (64) | register fails closed, no eviction |
//! | provider views | [`CW_PRESENT_MAX_NONTERMINAL_ITEMS`] (64) | [`CwHintProvider::with_views`] returns `None` past the cap |
//! | scene blocks per plan | [`CW_PRESENT_MAX_SCENE_BLOCKS`] (64) | deterministic shed of the iteration tail, [`ScenePresent::shed`] reports it |
//! | non-terminal items | [`CW_PRESENT_MAX_NONTERMINAL_ITEMS`] (64) | saturate + [`NonTerminalPresent::truncated`] |
//! | hint labels / text | `HINT_TARGET_MAX` / `HINT_TEXT_MAX_BYTES` (via [`HintBatch`](bitty_rich::hints::HintBatch)) | shed tail, never an overlay |
//! | fold ids | `FOLD_MAX` (via [`FoldState`](bitty_rich::blocks::FoldState)) | fold-to-closed fails closed |
//! | anchors per plan | input blocks (≤ `COMMAND_BLOCK_MAX`) | no allocation beyond the input slice |
//!
//! No I/O, no wall-clock, no randomness, no unsafe.

use bitty_rich::blocks::{CommandBlock, CommandId, FoldState, hidden_blocks, visible_blocks};
use bitty_rich::composer::{ComposerKeyEvent, ComposerSession, frame_submit};
use bitty_rich::hints::{
    DispatchError, DispatchOutcome, HintAction, HintBatch, HintRegistry, HintScope,
    collect_command_targets, collect_panel_targets, collect_view_targets, dispatch,
};
use bitty_rich::scene::{BlockId, Scene};
use bitty_term_state::State;
use bitty_ui::panel::ViewContent;
use bitty_ui::uitree::UiNodeId;
use bitty_ui::view::ViewId;

// ---------------------------------------------------------------------------
// Bounds (checked against the tree: no other `CW_PRESENT_*` exists)
// ---------------------------------------------------------------------------

/// Maximum hint providers registered on one [`CwHintEngine`].
///
/// Mirrors the provider-registry scale (`MAX_TARGET_PROVIDERS = 64` in
/// `bitty-ui`); registration past the cap fails closed.
pub const CW_PRESENT_MAX_PROVIDERS: usize = 64;

/// Maximum scene blocks admitted into one [`ScenePresent`] paint plan.
///
/// Mirrors `SCENE_MAX_BLOCKS_PER_TERMINAL` (64); the caller paint cap is
/// clamped to this, and the iteration tail is shed deterministically.
pub const CW_PRESENT_MAX_SCENE_BLOCKS: usize = 64;

/// Maximum views per [`CwHintProvider`] and items per [`NonTerminalPresent`].
///
/// Saturation cap with an explicit `truncated` flag, never silent loss.
pub const CW_PRESENT_MAX_NONTERMINAL_ITEMS: usize = 64;

// ---------------------------------------------------------------------------
// CW-01 (#980): fold toggle/expand/collapse into the present path
// ---------------------------------------------------------------------------

/// Present-path fold verb (CW-01 keymap-action surface, headless).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CwFoldAction {
    /// Flip the block's fold membership.
    Toggle,
    /// Ensure unfolded (idempotent).
    Expand,
    /// Ensure folded (idempotent, fails closed at the fold cap).
    Collapse,
}

/// Applies one [`CwFoldAction`] to the caller's [`FoldState`].
///
/// Returns `true` when the requested end state holds afterwards:
/// `Toggle` reports the post-flip membership, `Expand` always reports `true`
/// (unfolded holds trivially), and `Collapse` reports the insertion
/// (`false` when already folded or at the `FOLD_MAX` cap — either way the
/// block stays folded). Terminal truth is untouched.
#[must_use]
pub fn apply_fold_action(fold: &mut FoldState, id: CommandId, action: CwFoldAction) -> bool {
    match action {
        CwFoldAction::Toggle => {
            if fold.is_folded(id) {
                fold.unfold(id);
                false
            } else {
                fold.fold(id)
            }
        }
        CwFoldAction::Expand => {
            fold.unfold(id);
            true
        }
        CwFoldAction::Collapse => fold.fold(id),
    }
}

/// Fold projection for one present frame (CW-01 overlay projection).
///
/// Splits the frame's command blocks into visible/hidden id lists using the
/// caller's [`FoldState`]. Hidden rows are a paint concern only; the grid
/// and scrollback behind them are untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldPresent {
    /// Ids painted normally.
    pub visible: Vec<CommandId>,
    /// Ids hidden by the fold projection.
    pub hidden: Vec<CommandId>,
}

impl FoldPresent {
    /// Number of visible blocks.
    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// Number of folded-away blocks.
    #[must_use]
    pub fn hidden_count(&self) -> usize {
        self.hidden.len()
    }
}

/// Derives the [`FoldPresent`] projection for `blocks` under `fold`.
#[must_use]
pub fn fold_present(blocks: &[CommandBlock], fold: &FoldState) -> FoldPresent {
    FoldPresent {
        visible: visible_blocks(blocks, fold)
            .iter()
            .map(|block| block.id)
            .collect(),
        hidden: hidden_blocks(blocks, fold)
            .iter()
            .map(|block| block.id)
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// CW-05 (#984): semantic anchor identity + fold persistence ownership
// ---------------------------------------------------------------------------

/// Stable present-path anchor for one command block (CW-05).
///
/// `Zone` carries the `OSC 133` anchor ordinal behind the block's
/// [`CommandId`]; `Line` carries a scrollback line id supplied by the caller
/// as the documented fallback. There is deliberately no grid-row variant:
/// rows invalidate on resize/reflow/scroll, ordinals and line ids do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticAnchor {
    /// Preferred: `OSC 133` anchor ordinal (geometry-independent).
    Zone(u64),
    /// Fallback: scrollback line id (preserved across reflow).
    Line(u64),
}

impl SemanticAnchor {
    /// The ordinal or line id carried by this anchor.
    #[must_use]
    pub const fn key(self) -> u64 {
        match self {
            Self::Zone(ordinal) | Self::Line(ordinal) => ordinal,
        }
    }

    /// Whether this is the preferred ordinal anchor.
    #[must_use]
    pub const fn is_zone(self) -> bool {
        matches!(self, Self::Zone(_))
    }
}

/// Derives the preferred ordinal anchor for one command block.
///
/// Keys on the block's stable [`CommandId`] (itself the anchor ordinal),
/// never on a grid row.
#[must_use]
pub fn anchor_for_command(block: &CommandBlock) -> SemanticAnchor {
    SemanticAnchor::Zone(block.id.get())
}

/// Builds the fallback line anchor from a caller-supplied scrollback line id.
#[must_use]
pub fn anchor_line(line_id: u64) -> SemanticAnchor {
    SemanticAnchor::Line(line_id)
}

/// Serializes fold membership for persistence (CW-05 ownership).
///
/// The present layer owns persistence: it stores anchor ordinals (never grid
/// rows) and replays them through [`apply_fold_action`] on rehydrate. The
/// returned ordinals are bounded by `FOLD_MAX` by construction.
#[must_use]
pub fn persist_fold_ordinals(fold: &FoldState) -> Vec<u64> {
    fold.folded_ids().iter().map(|id| id.get()).collect()
}

// ---------------------------------------------------------------------------
// CW-02 (#981): hint overlay + dispatch in the present path
// ---------------------------------------------------------------------------

/// Builds the single annotation-layer batch for one present generation.
///
/// Thin wrapper over [`HintBatch::build`] so the present path names one
/// collection point; the batch rides the compositor as one layer and consumes
/// zero overlay slots (see [`present_overlay_cost`]).
#[must_use]
pub fn present_hint_batch(registry: &HintRegistry, generation: u64) -> HintBatch {
    HintBatch::build(generation, registry)
}

/// Overlay slots consumed by a hint batch in the present path: always `0`.
///
/// Contract anchor for the `4+1` bound: hint chrome paints as a single
/// annotation pass and never allocates, dismisses, or reorders overlay
/// entries.
#[must_use]
pub const fn present_overlay_cost(batch: &HintBatch) -> usize {
    batch.overlay_cost()
}

/// Dispatches `Action(Target)` from the present path.
///
/// Resolves `label` in `batch` and applies `action`, mutating only the
/// caller's [`FoldState`]. See [`dispatch`] for the fail-closed contract.
pub fn dispatch_present(
    batch: &HintBatch,
    fold: &mut FoldState,
    label: &str,
    action: HintAction,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch(batch, fold, label, action)
}

// ---------------------------------------------------------------------------
// CW-03 (#982): composer overlay + input routing + editor flag
// ---------------------------------------------------------------------------

/// Where one input event routes while the present frame is live (CW-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CwInputRoute {
    /// Composer is closed: bytes reach the PTY unchanged.
    Pty,
    /// Composer is open: the press feeds [`feed_present`], never the PTY.
    Composer,
}

/// Routes input for the current present frame around the composer session.
///
/// Closed (the default) routes to the PTY byte-identically; open routes to
/// the composer. There is no auto-enter path: opening is always explicit via
/// [`ComposerSession::open`].
#[must_use]
pub fn route_present_input(session: &ComposerSession) -> CwInputRoute {
    if session.is_open() {
        CwInputRoute::Composer
    } else {
        CwInputRoute::Pty
    }
}

/// Headless composer snapshot carried by the present frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerPresent {
    /// Whether the composer overlay paints this frame.
    pub open: bool,
    /// Draft bytes held in the buffer (≤ `COMPOSER_MAX_BYTES`).
    pub draft_bytes: usize,
}

impl ComposerPresent {
    /// Whether the modal composer overlay paints.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }
}

/// Snapshots the composer overlay state for one present frame.
#[must_use]
pub fn composer_present(session: &ComposerSession) -> ComposerPresent {
    ComposerPresent {
        open: session.is_open(),
        draft_bytes: session.content().len(),
    }
}

/// Owned outcome of feeding one press through the present path (CW-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwComposerFeed {
    /// Session closed: the caller must write the bytes to the PTY unchanged.
    PtyPassthrough,
    /// Printable text appended to the draft.
    Inserted,
    /// Newline appended to the draft.
    Newline,
    /// Submit frame ready for a single PTY write (session auto-closed).
    Submitted(Vec<u8>),
    /// Composer closed, draft preserved for reopen.
    Closed,
    /// Caller should run the external-editor round trip, then feed the result
    /// back via [`ComposerSession::apply_external_result`]. No process is
    /// spawned here: the present path records the request only.
    EditorRequested,
    /// Press matched no composer role; swallowed, draft untouched.
    Ignored,
    /// Draft cap hit; buffer and open-state untouched.
    TooLarge {
        /// Bytes the content would have had.
        wanted: usize,
    },
}

/// Feeds one key press through the present-path composer.
///
/// Closed sessions return [`CwComposerFeed::PtyPassthrough`] (the hard
/// boundary: normal-mode input is PTY byte-identical). Open sessions map the
/// [`ComposerSession`] outcome 1:1, including the submit frame
/// (`ESC[200~ + content + ESC[201~ + CR`) and the external-editor request
/// flag.
pub fn feed_present(session: &mut ComposerSession, ev: ComposerKeyEvent) -> CwComposerFeed {
    if !session.is_open() {
        return CwComposerFeed::PtyPassthrough;
    }
    match session.feed(ev) {
        Ok(outcome) => match outcome {
            bitty_rich::composer::ComposerFeedOutcome::Inserted => CwComposerFeed::Inserted,
            bitty_rich::composer::ComposerFeedOutcome::Newline => CwComposerFeed::Newline,
            bitty_rich::composer::ComposerFeedOutcome::Submitted(frame) => {
                CwComposerFeed::Submitted(frame)
            }
            bitty_rich::composer::ComposerFeedOutcome::Closed => CwComposerFeed::Closed,
            bitty_rich::composer::ComposerFeedOutcome::ExternalEditorRequested => {
                CwComposerFeed::EditorRequested
            }
            bitty_rich::composer::ComposerFeedOutcome::Ignored => CwComposerFeed::Ignored,
        },
        Err(bitty_rich::composer::ComposerFeedError::NotOpen) => CwComposerFeed::PtyPassthrough,
        Err(bitty_rich::composer::ComposerFeedError::TooLarge { wanted }) => {
            CwComposerFeed::TooLarge { wanted }
        }
    }
}

/// Frames draft content for submit without a session (PTY write helper).
///
/// Returns `None` (fail-closed, nothing emitted) when the content exceeds the
/// composer cap. See [`frame_submit`] for the byte-exact contract.
#[must_use]
pub fn submit_present(content: &str) -> Option<Vec<u8>> {
    frame_submit(content).ok()
}

// ---------------------------------------------------------------------------
// CW-04 (#983): cross-panel hint API — one engine owns labels/overlay/dispatch
// ---------------------------------------------------------------------------

/// One panel's contribution to a cross-panel hint collection (CW-04).
///
/// Carries raw leaf ids so this layer gains no widget dependency; the caller
/// resolves them back to panels/views at dispatch time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CwHintProvider {
    /// Raw panel id (the `bitty-ui` panel handle value).
    panel: u64,
    /// Raw view ids hosted under this panel.
    views: Vec<u64>,
}

impl CwHintProvider {
    /// Provider for a panel with no extra views.
    #[must_use]
    pub fn new(panel: u64) -> Self {
        Self {
            panel,
            views: Vec::new(),
        }
    }

    /// Provider for a panel plus its hosted views.
    ///
    /// Returns `None` (fail-closed) when `views` exceeds
    /// [`CW_PRESENT_MAX_NONTERMINAL_ITEMS`].
    #[must_use]
    pub fn with_views(panel: u64, views: &[u64]) -> Option<Self> {
        if views.len() > CW_PRESENT_MAX_NONTERMINAL_ITEMS {
            return None;
        }
        Some(Self {
            panel,
            views: views.to_vec(),
        })
    }

    /// Raw panel id contributed by this provider.
    #[must_use]
    pub const fn panel(&self) -> u64 {
        self.panel
    }

    /// Raw view ids contributed by this provider.
    #[must_use]
    pub fn views(&self) -> &[u64] {
        &self.views
    }
}

/// The single cross-panel hint engine (CW-04, P6 generalized from P3).
///
/// One engine owns provider registration, label allocation (via a single
/// [`HintRegistry`] per collection), the overlay batch, and dispatch. Panels
/// never allocate labels independently, so labels stay unique across panels
/// and the overlay stays one layer with zero overlay-slot cost.
#[derive(Debug, Clone, Default)]
pub struct CwHintEngine {
    providers: Vec<CwHintProvider>,
    include_commands: bool,
}

impl CwHintEngine {
    /// Empty engine with no providers and no command targets.
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            include_commands: false,
        }
    }

    /// Sets whether command-block targets join each collection.
    #[must_use]
    pub fn with_commands(mut self, include: bool) -> Self {
        self.include_commands = include;
        self
    }

    /// Registers one panel provider.
    ///
    /// Returns `false` (fail-closed, engine untouched) when the engine holds
    /// [`CW_PRESENT_MAX_PROVIDERS`] providers already or the panel is
    /// already registered.
    pub fn register_provider(&mut self, provider: CwHintProvider) -> bool {
        if self.providers.len() >= CW_PRESENT_MAX_PROVIDERS {
            return false;
        }
        if self.providers.iter().any(|p| p.panel == provider.panel) {
            return false;
        }
        self.providers.push(provider);
        true
    }

    /// Number of registered providers.
    #[must_use]
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Whether the engine collects nothing (no providers, no commands).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && !self.include_commands
    }

    /// Collects one cross-panel batch for `generation`.
    ///
    /// Builds a single [`HintRegistry`] (command targets once when enabled,
    /// then every provider's panel + views under `scope`), then allocates
    /// the single overlay batch from it. Deterministic per target set;
    /// over-cap shedding follows the [`HintBatch`] tail-shed rule.
    #[must_use]
    pub fn collect(&self, state: &State, scope: HintScope, generation: u64) -> HintBatch {
        let mut registry = HintRegistry::new();
        if self.include_commands {
            collect_command_targets(&mut registry, state, scope);
        }
        for provider in &self.providers {
            collect_panel_targets(&mut registry, &[provider.panel], scope);
            collect_view_targets(&mut registry, &provider.views, scope);
        }
        HintBatch::build(generation, &registry)
    }
}

// ---------------------------------------------------------------------------
// CW-06 (#985): consume the rich scene in the present path
// ---------------------------------------------------------------------------

/// Paint-budget view of the rich scene for one present frame (CW-06).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScenePresent {
    /// Admitted block ids in scene iteration order.
    pub block_ids: Vec<BlockId>,
    /// Total scene-graph nodes admitted (paint work estimate).
    pub node_total: usize,
    /// Total text bytes admitted (paint work estimate).
    pub text_total: usize,
    /// Blocks shed by the paint cap.
    pub shed: usize,
}

impl ScenePresent {
    /// Number of admitted blocks.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.block_ids.len()
    }
}

/// Consumes [`Scene`] blocks into the present paint plan (CW-06).
///
/// Admits at most `paint_cap` blocks (clamped to
/// [`CW_PRESENT_MAX_SCENE_BLOCKS`]) in scene iteration order and sheds the
/// tail deterministically, reporting the count in [`ScenePresent::shed`].
/// Scene admission bounds (`SCN-1..5`) are enforced at insert time; the
/// present path only budgets paint work and never re-validates content.
#[must_use]
pub fn consume_scene_present(scene: &Scene, paint_cap: usize) -> ScenePresent {
    let cap = paint_cap.min(CW_PRESENT_MAX_SCENE_BLOCKS);
    let mut present = ScenePresent::default();
    for block in scene.iter() {
        if present.block_ids.len() >= cap {
            break;
        }
        present.node_total = present.node_total.saturating_add(block.node_count());
        present.text_total = present.text_total.saturating_add(block.text_bytes());
        present.block_ids.push(block.id());
    }
    present.shed = scene.len().saturating_sub(present.block_ids.len());
    present
}

// ---------------------------------------------------------------------------
// CW-11 (#990): non-terminal panel content beyond the character grid
// ---------------------------------------------------------------------------

/// Non-terminal payload painted beyond the character grid (CW-11).
///
/// Covers [`ViewContent::Panel`], `Browser`, and `Rich` leaves: content the
/// grid pipeline cannot express (canvas display lists, rich blocks, embedded
/// surfaces). Addressed by the canonical [`UiNodeId`] — this module defines
/// no id of its own — and joined to the terminal binding by the runtime. Like
/// hint batches, it consumes zero overlay slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonTerminalPresent {
    /// Raw panel/browser/rich handle behind the leaf.
    pub panel: u64,
    /// Canonical UI node identity for the payload.
    pub node: UiNodeId,
    /// Item count, saturated at [`CW_PRESENT_MAX_NONTERMINAL_ITEMS`].
    pub items: usize,
    /// Whether `items` was saturated.
    pub truncated: bool,
}

impl NonTerminalPresent {
    /// Overlay slots consumed: always `0` (never touches the `4+1` bound).
    #[must_use]
    pub const fn overlay_cost(&self) -> usize {
        0
    }
}

/// Maps leaf content to its non-terminal present payload (CW-11).
///
/// `Panel`, `Browser`, and `Rich` leaves yield a payload; `Terminal` and
/// `Empty` yield `None` (the grid pipeline owns those). `items` is the
/// caller-estimated payload size and is saturated at
/// [`CW_PRESENT_MAX_NONTERMINAL_ITEMS`] with [`NonTerminalPresent::truncated`]
/// set, never silently dropped without a flag.
#[must_use]
pub fn nonterminal_for_content(
    content: ViewContent,
    node: UiNodeId,
    items: usize,
) -> Option<NonTerminalPresent> {
    let panel = match content {
        ViewContent::Panel(id) => id.get(),
        ViewContent::Browser(id) => id.get(),
        ViewContent::Rich(raw) => raw,
        ViewContent::Terminal(_) | ViewContent::Empty => return None,
    };
    if items > CW_PRESENT_MAX_NONTERMINAL_ITEMS {
        Some(NonTerminalPresent {
            panel,
            node,
            items: CW_PRESENT_MAX_NONTERMINAL_ITEMS,
            truncated: true,
        })
    } else {
        Some(NonTerminalPresent {
            panel,
            node,
            items,
            truncated: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Top-level present plan: all seven slices, one frame
// ---------------------------------------------------------------------------

/// Inputs for one [`plan_present`] derivation (bundled: one argument).
pub struct CwPresentInputs<'a> {
    /// Leaf this plan paints.
    pub view: ViewId,
    /// Damage/collection generation this plan is derived for.
    pub generation: u64,
    /// Command blocks visible to this frame.
    pub blocks: &'a [CommandBlock],
    /// Caller-owned fold projection.
    pub fold: &'a FoldState,
    /// Pre-collected hint batch for this generation.
    pub hints: &'a HintBatch,
    /// Composer session (overlay + routing source).
    pub composer: &'a ComposerSession,
    /// Rich scene (paint-budget source).
    pub scene: &'a Scene,
    /// Leaf content (terminal vs non-terminal split).
    pub content: ViewContent,
    /// Canonical UI node identity for non-terminal payloads.
    pub node: UiNodeId,
}

/// One frame's CW present enrichment: all seven slices composed.
///
/// Derived once per [`PresentFrame`](crate::runtime::PresentFrame) at refresh
/// from headless inputs. Pure (except the caller's fold, which this function
/// never mutates — dispatch goes through [`dispatch_present`]), bounded, and
/// deterministic per input tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CwPresentPlan {
    /// Leaf this plan paints.
    pub view: ViewId,
    /// Generation this plan was derived for.
    pub generation: u64,
    /// Visible command ids (fold projection).
    pub visible_command_ids: Vec<CommandId>,
    /// Folded-away command ids (paint-hidden only).
    pub hidden_command_ids: Vec<CommandId>,
    /// Semantic anchors, one per input block, in input order.
    pub anchors: Vec<SemanticAnchor>,
    /// Allocated hint labels this frame.
    pub hint_labels: usize,
    /// Hint targets shed by the label caps.
    pub hint_shed: usize,
    /// Hint overlay slots consumed: always `0`.
    pub hint_overlay_cost: usize,
    /// Whether the composer overlay paints.
    pub composer_open: bool,
    /// Composer draft bytes.
    pub composer_draft_bytes: usize,
    /// Where input routes this frame.
    pub input_route: CwInputRoute,
    /// Rich-scene paint budget.
    pub scene: ScenePresent,
    /// Non-terminal payload (`None` for grid-owned leaves).
    pub nonterminal: Option<NonTerminalPresent>,
}

/// Derives the [`CwPresentPlan`] for one present frame.
#[must_use]
pub fn plan_present(inputs: &CwPresentInputs<'_>) -> CwPresentPlan {
    let projection = fold_present(inputs.blocks, inputs.fold);
    CwPresentPlan {
        view: inputs.view,
        generation: inputs.generation,
        visible_command_ids: projection.visible,
        hidden_command_ids: projection.hidden,
        anchors: inputs.blocks.iter().map(anchor_for_command).collect(),
        hint_labels: inputs.hints.len(),
        hint_shed: inputs.hints.shed,
        hint_overlay_cost: present_overlay_cost(inputs.hints),
        composer_open: inputs.composer.is_open(),
        composer_draft_bytes: inputs.composer.content().len(),
        input_route: route_present_input(inputs.composer),
        scene: consume_scene_present(inputs.scene, CW_PRESENT_MAX_SCENE_BLOCKS),
        nonterminal: nonterminal_for_content(inputs.content, inputs.node, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_rich::blocks::{CommandState, SemanticRange};
    use bitty_rich::composer::ComposerFeedError;
    use bitty_rich::hints::{HintActions, HintAnchor, HintKind};
    use bitty_rich::scene::{BlockAnchor, RichBlock, SceneNode, ScrollBehavior, StyledSpan};
    use bitty_rich::shell::CommandRegion;
    use bitty_ui::panel::PanelId;

    fn test_block(anchor: u64) -> CommandBlock {
        CommandBlock {
            id: CommandId(anchor),
            command_range: SemanticRange::new(anchor, anchor),
            output_range: None,
            cwd: None,
            exit_code: None,
            state: CommandState::Completed,
            region: CommandRegion {
                prompt_start: Some(anchor),
                input_start: None,
                output_start: None,
                output_end: None,
                exit_code: None,
                prompt_row: None,
                input_row: None,
                output_row: None,
                output_end_row: None,
            },
        }
    }

    fn test_scene_block(id: u64, text: &str) -> RichBlock {
        RichBlock::new(
            BlockId(id),
            BlockAnchor::Zone(id),
            SceneNode::Text(StyledSpan {
                text: text.to_string(),
                bold: false,
                italic: false,
            }),
            ScrollBehavior::Inline,
            1,
            1,
            1,
        )
        .expect("test block fits SCN-1..3")
    }

    fn empty_batch() -> HintBatch {
        HintBatch::build(0, &HintRegistry::new())
    }

    #[test]
    fn fold_toggle_applies_to_present_projection() {
        let blocks = vec![test_block(1), test_block(2)];
        let mut fold = FoldState::new();
        assert!(apply_fold_action(
            &mut fold,
            CommandId(1),
            CwFoldAction::Toggle
        ));
        let projection = fold_present(&blocks, &fold);
        assert_eq!(projection.visible, vec![CommandId(2)]);
        assert_eq!(projection.hidden, vec![CommandId(1)]);
        assert_eq!(projection.visible_count(), 1);
        assert_eq!(projection.hidden_count(), 1);
    }

    #[test]
    fn fold_expand_collapse_are_idempotent() {
        let mut fold = FoldState::new();
        assert!(apply_fold_action(
            &mut fold,
            CommandId(5),
            CwFoldAction::Collapse
        ));
        assert!(fold.is_folded(CommandId(5)));
        assert!(!apply_fold_action(
            &mut fold,
            CommandId(5),
            CwFoldAction::Collapse
        ));
        assert!(fold.is_folded(CommandId(5)));
        assert!(apply_fold_action(
            &mut fold,
            CommandId(5),
            CwFoldAction::Expand
        ));
        assert!(!fold.is_folded(CommandId(5)));
        assert!(apply_fold_action(
            &mut fold,
            CommandId(5),
            CwFoldAction::Expand
        ));
        assert!(apply_fold_action(
            &mut fold,
            CommandId(5),
            CwFoldAction::Toggle
        ));
        assert!(fold.is_folded(CommandId(5)));
    }

    #[test]
    fn hint_batch_is_single_layer_zero_overlay_cost() {
        let mut registry = HintRegistry::new();
        let scope = HintScope(1);
        assert!(
            registry
                .register(
                    HintKind::Panel,
                    HintAnchor::Panel(7),
                    scope,
                    HintActions::default_for(HintKind::Panel),
                )
                .is_some()
        );
        let batch = present_hint_batch(&registry, 3);
        assert_eq!(batch.len(), 1);
        assert_eq!(present_overlay_cost(&batch), 0);
        assert_eq!(batch.overlay_cost(), 0);
    }

    #[test]
    fn hint_dispatch_toggles_fold_through_present() {
        let mut registry = HintRegistry::new();
        let scope = HintScope(0);
        assert!(
            registry
                .register(
                    HintKind::CommandBlock,
                    HintAnchor::Command(CommandId(9)),
                    scope,
                    HintActions::default_for(HintKind::CommandBlock),
                )
                .is_some()
        );
        let batch = present_hint_batch(&registry, 1);
        let label = batch.labels[0].label.clone();
        let mut fold = FoldState::new();
        let outcome = dispatch_present(&batch, &mut fold, &label, HintAction::ToggleFold)
            .expect("toggle fold dispatches");
        assert_eq!(
            outcome,
            DispatchOutcome::FoldToggled {
                id: CommandId(9),
                folded: true,
            }
        );
        assert!(fold.is_folded(CommandId(9)));
        assert_eq!(
            dispatch_present(&batch, &mut fold, "zzz-missing", HintAction::Jump),
            Err(DispatchError::UnknownLabel)
        );
    }

    #[test]
    fn cross_panel_engine_owns_labels_across_panels() {
        let state = State::new();
        let mut engine = CwHintEngine::new().with_commands(false);
        assert!(engine.is_empty());
        assert!(
            engine.register_provider(CwHintProvider::with_views(1, &[11, 12]).expect("fits cap"))
        );
        assert!(engine.register_provider(CwHintProvider::new(2)));
        assert_eq!(engine.provider_count(), 2);
        assert!(!engine.is_empty());
        let batch = engine.collect(&state, HintScope(4), 9);
        assert_eq!(batch.generation, 9);
        assert_eq!(batch.len(), 4);
        let mut labels: Vec<&str> = batch.labels.iter().map(|l| l.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 4);
        assert_eq!(present_overlay_cost(&batch), 0);
    }

    #[test]
    fn engine_rejects_duplicate_provider_and_oversize_views() {
        let mut engine = CwHintEngine::new();
        assert!(engine.register_provider(CwHintProvider::new(1)));
        assert!(!engine.register_provider(CwHintProvider::new(1)));
        let too_many = vec![0u64; CW_PRESENT_MAX_NONTERMINAL_ITEMS + 1];
        assert!(CwHintProvider::with_views(3, &too_many).is_none());
        assert_eq!(engine.provider_count(), 1);
    }

    #[test]
    fn composer_closed_routes_to_pty_passthrough() {
        let mut session = ComposerSession::new();
        assert_eq!(route_present_input(&session), CwInputRoute::Pty);
        assert_eq!(
            feed_present(&mut session, ComposerKeyEvent::printable('x')),
            CwComposerFeed::PtyPassthrough
        );
        let snapshot = composer_present(&session);
        assert!(!snapshot.is_open());
        assert_eq!(snapshot.draft_bytes, 0);
        assert_eq!(session.content(), "");
    }

    #[test]
    fn composer_open_submit_frames_bracketed_paste() {
        let mut session = ComposerSession::new();
        session.open();
        assert_eq!(route_present_input(&session), CwInputRoute::Composer);
        session
            .apply_external_result("echo hi")
            .expect("fits composer cap");
        assert_eq!(composer_present(&session).draft_bytes, 7);
        let outcome = feed_present(&mut session, ComposerKeyEvent::ctrl_enter());
        match outcome {
            CwComposerFeed::Submitted(frame) => {
                assert!(frame.starts_with(b"\x1b[200~"));
                assert!(frame.ends_with(b"\x1b[201~\r"));
                assert!(frame.windows(7).any(|w| w == b"echo hi"));
            }
            other => panic!("expected submit frame, got {other:?}"),
        }
        assert_eq!(route_present_input(&session), CwInputRoute::Pty);
        assert_eq!(
            submit_present("echo hi").expect("fits cap").as_slice(),
            b"\x1b[200~echo hi\x1b[201~\r",
        );
        assert_eq!(
            session.feed(ComposerKeyEvent::printable('y')),
            Err(ComposerFeedError::NotOpen)
        );
    }

    #[test]
    fn composer_editor_request_is_flag_only() {
        let mut session = ComposerSession::new();
        session.open();
        assert_eq!(
            feed_present(&mut session, ComposerKeyEvent::alt_e()),
            CwComposerFeed::EditorRequested
        );
        assert!(session.is_open());
    }

    #[test]
    fn anchor_identity_is_ordinal_never_grid_row() {
        let block = test_block(42);
        let anchor = anchor_for_command(&block);
        assert_eq!(anchor, SemanticAnchor::Zone(42));
        assert!(anchor.is_zone());
        assert_eq!(anchor.key(), 42);
        let fallback = anchor_line(7);
        assert_eq!(fallback, SemanticAnchor::Line(7));
        assert!(!fallback.is_zone());
        assert_ne!(anchor, fallback);
    }

    #[test]
    fn fold_persistence_roundtrips_ordinals() {
        let mut fold = FoldState::new();
        assert!(apply_fold_action(
            &mut fold,
            CommandId(3),
            CwFoldAction::Collapse
        ));
        assert!(apply_fold_action(
            &mut fold,
            CommandId(8),
            CwFoldAction::Collapse
        ));
        let persisted = persist_fold_ordinals(&fold);
        assert_eq!(persisted, vec![3, 8]);
        let mut restored = FoldState::new();
        for ordinal in persisted {
            assert!(restored.fold(CommandId(ordinal)));
        }
        assert!(restored.is_folded(CommandId(3)));
        assert!(restored.is_folded(CommandId(8)));
    }

    #[test]
    fn scene_consume_sheds_tail_over_paint_cap() {
        let mut scene = Scene::new();
        for id in 1..=3u64 {
            scene
                .insert(test_scene_block(id, "hello"))
                .expect("fits scene caps");
        }
        let full = consume_scene_present(&scene, CW_PRESENT_MAX_SCENE_BLOCKS);
        assert_eq!(full.block_count(), 3);
        assert_eq!(full.shed, 0);
        assert_eq!(full.node_total, 3);
        assert_eq!(full.text_total, 15);
        let capped = consume_scene_present(&scene, 2);
        assert_eq!(capped.block_count(), 2);
        assert_eq!(capped.shed, 1);
        assert_eq!(capped.block_ids, vec![BlockId(1), BlockId(2)]);
        let empty = consume_scene_present(&Scene::new(), CW_PRESENT_MAX_SCENE_BLOCKS);
        assert_eq!(empty.block_count(), 0);
        assert_eq!(empty.shed, 0);
    }

    #[test]
    fn nonterminal_panel_present_beyond_grid_zero_cost() {
        let node = UiNodeId::new(3);
        let panel = nonterminal_for_content(ViewContent::Panel(PanelId::new(9)), node, 5)
            .expect("panel is non-terminal");
        assert_eq!(panel.panel, 9);
        assert_eq!(panel.node, node);
        assert_eq!(panel.items, 5);
        assert!(!panel.truncated);
        assert_eq!(panel.overlay_cost(), 0);
        let saturated = nonterminal_for_content(
            ViewContent::Rich(2),
            node,
            CW_PRESENT_MAX_NONTERMINAL_ITEMS + 1,
        )
        .expect("rich is non-terminal");
        assert_eq!(saturated.items, CW_PRESENT_MAX_NONTERMINAL_ITEMS);
        assert!(saturated.truncated);
        assert!(nonterminal_for_content(ViewContent::Terminal(1), node, 0).is_none());
        assert!(nonterminal_for_content(ViewContent::Empty, node, 0).is_none());
    }

    #[test]
    fn plan_present_composes_all_seven_slices() {
        let blocks = vec![test_block(1), test_block(2)];
        let mut fold = FoldState::new();
        assert!(apply_fold_action(
            &mut fold,
            CommandId(2),
            CwFoldAction::Collapse
        ));
        let batch = empty_batch();
        let composer = ComposerSession::new();
        let scene = Scene::new();
        let inputs = CwPresentInputs {
            view: ViewId::new(1),
            generation: 3,
            blocks: &blocks,
            fold: &fold,
            hints: &batch,
            composer: &composer,
            scene: &scene,
            content: ViewContent::Panel(PanelId::new(9)),
            node: UiNodeId::new(3),
        };
        let plan = plan_present(&inputs);
        assert_eq!(plan.view, ViewId::new(1));
        assert_eq!(plan.generation, 3);
        assert_eq!(plan.visible_command_ids, vec![CommandId(1)]);
        assert_eq!(plan.hidden_command_ids, vec![CommandId(2)]);
        assert_eq!(
            plan.anchors,
            vec![SemanticAnchor::Zone(1), SemanticAnchor::Zone(2)]
        );
        assert_eq!(plan.hint_labels, 0);
        assert_eq!(plan.hint_overlay_cost, 0);
        assert!(!plan.composer_open);
        assert_eq!(plan.input_route, CwInputRoute::Pty);
        assert_eq!(plan.scene.block_count(), 0);
        assert!(plan.nonterminal.is_some());
    }
}
