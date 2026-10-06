//! CW render-path present enrichment (CTX-0687; live since CTX-0700).
//!
//! Headless present-path enrichment over the CW render-path issues. It
//! consumes the accepted headless models — fold, hints, anchors,
//! rich scene, non-terminal panel content — into a single bounded
//! [`CwPresentPlan`] derived per [`PresentFrame`](crate::runtime::PresentFrame)
//! at refresh, without touching grid truth, GPU, PTY, or the filesystem.
//!
//! # Per-issue mapping
//!
//! | Issue | Slice | Entry point |
//! |---|---|---|
//! | #980 CW-01 | fold toggle/expand/collapse into present | [`CwFoldAction`] + [`apply_fold_action`] + [`fold_present`] |
//! | #981 CW-02 | hint overlay + dispatch | [`present_hint_batch`] + [`present_overlay_cost`] + [`hint_overlay_present`] + [`dispatch_present`] |
//! | #982 CW-03 | retired (E-CUT-4, CTX-0968): composer overlay deleted, plugin owns editing UX | — |
//! | #983 CW-04 | cross-panel hint API, one engine | [`CwHintEngine`] + [`CwHintProvider`] |
//! | #984 CW-05 | semantic anchor identity + fold persistence | [`SemanticAnchor`] + [`anchor_for_command`] + [`persist_fold_ordinals`] |
//! | #985 CW-06 | consume rich scene in present | [`consume_scene_present`] + [`ScenePresent`] |
//! | #990 CW-11 | non-terminal content beyond the grid | [`NonTerminalPresent`] + [`nonterminal_for_content`] |
//!
//! # Decision status and live wiring
//!
//! The owner decisions behind these issues are Accepted: `OQ-050`
//! (scrollback identity), `OQ-051` (panel-is-not-terminal), and
//! `OQ-088`/`OQ-089` (hint leadership; the hint engine is the OQ-089
//! targeting mechanism). The module is wired into the live path: [`Runtime`](crate::Runtime)
//! owns the single [`CwHintEngine`] and fold state
//! (`runtime::cw_live`), and the app binds the fold and hint
//! verbs through `bitty-terminal`'s `chrome_keys` keymap dispatch. The
//! composer overlay (CW-03) is retired (E-CUT-4, CTX-0968): the plugin owns
//! editing UX via overlay/capture/submit/editor host operations. No editor
//! process is spawned and no grid write happens here.
//!
//! # Terminal truth
//!
//! The only mutation in the system is the caller's [`FoldState`](bitty_rich::blocks::FoldState)
//! via [`apply_fold_action`] / [`dispatch_present`]. Grid, scrollback,
//! zones, clipboard, and snapshots are never written.
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
//! | hint overlay entries | batch labels 1:1 (via [`HintOverlayPresent`]) | same shed as the batch, zero overlay slots |
//! | fold ids | `FOLD_MAX` (via [`FoldState`](bitty_rich::blocks::FoldState)) | fold-to-closed fails closed |
//! | anchors per plan | input blocks (≤ `COMMAND_BLOCK_MAX`) | no allocation beyond the input slice |
//!
//! No I/O, no wall-clock, no randomness, no unsafe.

use bitty_rich::blocks::{CommandBlock, CommandId, FoldState, hidden_blocks, visible_blocks};
use bitty_rich::hints::{
    DispatchError, DispatchOutcome, HintAction, HintAnchor, HintBatch, HintKind, HintRegistry,
    HintScope, collect_command_targets, collect_link_targets, collect_panel_targets,
    collect_view_targets, dispatch, dispatch_link,
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

// ---------------------------------------------------------------------------
// CW-02 (#981): hint overlay paint payload — the single annotation pass
// ---------------------------------------------------------------------------

/// One paint entry of the hint overlay: the label glyphs plus the target
/// identity the render path resolves to cells (CTX-0735, #981).
///
/// Data only — no pixels, no overlay allocation. The compositor paints every
/// entry in one annotation pass alongside selection/IME; the `4+1` overlay
/// bound is untouched (see [`HintOverlayPresent::overlay_cost`]). Label
/// order follows the batch (allocation rank). The render path resolves
/// `anchor` to viewport cells; command anchors are ordinals (never grid
/// rows), so OQ-050 row anchoring cannot reshape this payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintOverlayEntry {
    /// Label glyphs painted for this target (`a`, `s`, ..., `aa`, ...).
    pub label: String,
    /// Object kind (lets paint style commands vs leaves differently).
    pub kind: HintKind,
    /// Stable anchor the label addresses (never a grid row).
    pub anchor: HintAnchor,
}

/// Single batched hint overlay for one present frame (CTX-0735, #981).
///
/// Exactly the live [`HintBatch`] labels as paint data: one layer, never one
/// overlay per label. Bounded by the batch itself (`HINT_TARGET_MAX` /
/// `HINT_TEXT_MAX_BYTES`); [`shed`](Self::shed) mirrors the batch shed
/// count so paint and dispatch agree on what is addressable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HintOverlayPresent {
    /// Paint entries in batch label order (allocation rank).
    pub entries: Vec<HintOverlayEntry>,
    /// Targets shed by the label caps (mirrors [`HintBatch::shed`]).
    pub shed: usize,
}

impl HintOverlayPresent {
    /// Number of paint entries (at most `HINT_TARGET_MAX`, via the batch).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the overlay paints nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Overlay slots consumed: always `0` (never touches the `4+1` bound).
    #[must_use]
    pub const fn overlay_cost(&self) -> usize {
        0
    }
}

/// Derives the one-frame hint overlay from the live batch (CTX-0735, #981).
///
/// A 1:1 projection of [`HintBatch::labels`] in label order with the batch
/// shed count carried over: paint shows exactly what dispatch can resolve,
/// no more, no less. Pure and headless; the caller passes the armed
/// session's batch (or the plan input) so overlay and dispatch share one
/// authority.
#[must_use]
pub fn hint_overlay_present(batch: &HintBatch) -> HintOverlayPresent {
    HintOverlayPresent {
        entries: batch
            .labels()
            .iter()
            .map(|item| HintOverlayEntry {
                label: item.label.clone(),
                kind: item.kind,
                anchor: item.anchor,
            })
            .collect(),
        shed: batch.shed,
    }
}

/// One resolved hint-overlay paint cell: the leaf frame plus the viewport
/// cell the label pill covers (CTX-0751, #1344).
///
/// The terminal present path derives these from the armed session's
/// [`HintOverlayPresent`] once per frame and paints each as a single-cell-
/// high pill (label glyphs over a theme fill) in the annotation pass. Pure
/// data: positions are viewport cells, never pixels, so the compositor owns
/// all geometry. Fail-closed by construction — unresolvable anchors simply
/// yield no cell (see [`Runtime::cw_hint_overlay_cells`](crate::runtime::Runtime::cw_hint_overlay_cells)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintOverlayCell {
    /// Leaf frame the pill paints in.
    pub view: ViewId,
    /// Viewport column of the pill's left edge (cells).
    pub col: u16,
    /// Viewport row of the pill (cells).
    pub row: u16,
    /// Label glyphs painted (`a`, `s`, ..., `aa`, ...).
    pub label: String,
}

impl HintOverlayCell {
    /// Pill width in cells (labels are ASCII lowercase, one cell each).
    #[must_use]
    pub fn width_cells(&self) -> usize {
        self.label.chars().count()
    }
}

/// Dispatches `Action(Target)` from the present path.
///
/// Resolves `label` in `batch` and applies `action`, mutating only the
/// caller's [`FoldState`]. See [`dispatch`] for the fail-closed contract.
/// Link labels fail closed here ([`DispatchError::LinkNeedsState`]);
/// callers with live truth use [`dispatch_link_present`] instead.
pub fn dispatch_present(
    batch: &HintBatch,
    fold: &mut FoldState,
    label: &str,
    action: HintAction,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch(batch, fold, label, action)
}

/// Dispatches a link `Action(Target)` against live terminal truth
/// (CTX-0840, #1395).
///
/// Same contract as [`dispatch_present`] plus the [`dispatch_link`]
/// stateful step: the URI is re-resolved from `state` at dispatch time,
/// so a stale cell or an evicted hyperlink fails closed instead of
/// opening a substitute target.
pub fn dispatch_link_present(
    batch: &HintBatch,
    fold: &mut FoldState,
    state: &State,
    label: &str,
    action: HintAction,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch_link(batch, fold, state, label, action)
}

/// Outcome of feeding one letter to an armed hint interaction (CW-02 live
/// path, CTX-0723 issue #981).
///
/// The app maps each keystroke while the Leader window is armed to one
/// [`Runtime::cw_hint_push_key`](crate::runtime::Runtime::cw_hint_push_key)
/// call: the first letter selects the operator, following letters accumulate
/// the label, and the buffer dispatches once it resolves exactly with no
/// longer label extending it (prefix completion, so `a` fires immediately
/// while `aa` waits for its second keystroke).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintKeyOutcome {
    /// Operator or label prefix accepted; more keystrokes needed.
    NeedMore,
    /// Label completed uniquely and dispatched; the session is disarmed.
    Dispatched(DispatchOutcome),
    /// Key rejected: session disarmed, unknown operator, non-letter,
    /// overlong label, or a label buffer no target extends. The caller
    /// reports loudly and either retries the label or disarms.
    Invalid {
        /// The rejected keystroke.
        key: char,
    },
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

    /// Unregisters the provider for `panel` (CTX-0723, #983).
    ///
    /// Dispose symmetry for [`register_provider`](Self::register_provider):
    /// a disposed panel must stop contributing targets, or its labels would
    /// outlive the leaf they point at. Returns `true` when a provider was
    /// removed; `false` (no-op) when `panel` was never registered.
    pub fn unregister_provider(&mut self, panel: u64) -> bool {
        let before = self.providers.len();
        self.providers.retain(|p| p.panel != panel);
        self.providers.len() != before
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
    /// then every provider's panel + views under `scope`, then safe OSC 8
    /// link targets from the live grid via [`collect_link_targets`]),
    /// then allocates the single overlay batch from it. Deterministic per
    /// target set; over-cap shedding follows the [`HintBatch`] tail-shed
    /// rule.
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
        // CTX-0840 (#1395): OSC 8 links join every collection from the
        // live grid (safe schemes only; hostile/stale spans never admit).
        // Providers stay the explicit panel/view source; links are grid
        // truth, so they need no registration.
        collect_link_targets(&mut registry, state, scope);
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
    /// Rich scene (paint-budget source).
    pub scene: &'a Scene,
    /// Leaf content (terminal vs non-terminal split).
    pub content: ViewContent,
    /// Canonical UI node identity for non-terminal payloads.
    pub node: UiNodeId,
}

/// One frame's CW present enrichment: fold, hints, scene, non-terminal
/// composed (composer retired, E-CUT-4).
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
    /// One-frame hint overlay paint payload (CTX-0735, #981): the batch
    /// labels as a single annotation pass (zero overlay slots).
    pub hint_overlay: HintOverlayPresent,
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
        hint_overlay: hint_overlay_present(inputs.hints),
        scene: consume_scene_present(inputs.scene, CW_PRESENT_MAX_SCENE_BLOCKS),
        nonterminal: nonterminal_for_content(inputs.content, inputs.node, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_rich::blocks::{CommandState, SemanticRange};
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
    fn hint_overlay_projects_batch_labels_one_to_one() {
        // CTX-0735 (#981): the overlay paints exactly what dispatch can
        // resolve — same labels in the same order, same anchors, same shed,
        // zero overlay slots.
        let mut registry = HintRegistry::new();
        let scope = HintScope(1);
        for panel in [7u64, 3u64] {
            assert!(
                registry
                    .register(
                        HintKind::Panel,
                        HintAnchor::Panel(panel),
                        scope,
                        HintActions::default_for(HintKind::Panel),
                    )
                    .is_some()
            );
        }
        let batch = present_hint_batch(&registry, 3);
        assert_eq!(batch.len(), 2);
        let overlay = hint_overlay_present(&batch);
        assert_eq!(overlay.len(), 2);
        assert!(!overlay.is_empty());
        assert_eq!(overlay.overlay_cost(), 0);
        assert_eq!(overlay.shed, batch.shed);
        let batch_labels: Vec<&str> = batch.labels().iter().map(|l| l.label.as_str()).collect();
        let paint_labels: Vec<&str> = overlay.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(paint_labels, batch_labels, "paint follows label order");
        for (entry, item) in overlay.entries.iter().zip(batch.labels().iter()) {
            assert_eq!(entry.kind, item.kind);
            assert_eq!(entry.anchor, item.anchor);
        }
    }

    #[test]
    fn hint_overlay_empty_batch_paints_nothing() {
        let overlay = hint_overlay_present(&empty_batch());
        assert!(overlay.is_empty());
        assert_eq!(overlay.len(), 0);
        assert_eq!(overlay.overlay_cost(), 0);
        assert_eq!(overlay.shed, 0);
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
    fn plan_present_composes_slices_without_composer() {
        let blocks = vec![test_block(1), test_block(2)];
        let mut fold = FoldState::new();
        assert!(apply_fold_action(
            &mut fold,
            CommandId(2),
            CwFoldAction::Collapse
        ));
        let batch = empty_batch();
        let scene = Scene::new();
        let inputs = CwPresentInputs {
            view: ViewId::new(1),
            generation: 3,
            blocks: &blocks,
            fold: &fold,
            hints: &batch,
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
        assert_eq!(plan.scene.block_count(), 0);
        assert!(plan.nonterminal.is_some());
    }
}
