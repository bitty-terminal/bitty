//! Hint Mode: label allocator + annotation layer + action dispatcher
//! (CTX-0226, 008 route step 3 / P3).
//!
//! Design inputs (read-only): `recording/research/008.md` sections 4-10 and
//! the `flash.nvim` snapshot (rev `5f0f270`, Apache-2.0) at
//! `recording/references/flash.nvim`. flash.nvim is a **design reference
//! only**: the label ordering below is re-derived for Bitty (home-row-first
//! over the terminal keyboard) and no reference code is copied or adapted.
//!
//! # What this module is
//!
//! The third step of the 008 route (P1 anchoring + P2 fold MVP merged in
//! #390; this is P3). It turns the [`CommandBlock`](crate::blocks::CommandBlock)
//! list and the Panel/View leaf sets into keyboard-addressable targets:
//!
//! ```text
//! providers -> HintRegistry -> allocate_labels -> HintBatch -> dispatch
//!     (targets)      (labels)      (one layer)    (Action(Target))
//! ```
//!
//! * **Target + Action separation** (008 section 5): picking a label only
//!   selects a [`HintTarget`]; the operator key selects the [`HintAction`],
//!   so `Action(Target)` covers Jump, Focus, Toggle-fold, Expand, Collapse,
//!   and Copy without one mode per verb.
//! * **Operator-style chords** (008 section 6): `operator + label`, e.g.
//!   `j <label>` jumps, `z <label>` toggles a fold, `y <label>` copies.
//!   Chords are consumed **only while a [`HintSession`] is armed**; arming
//!   fails closed when any operator key collides with a bare key already
//!   bound in the caller's keymap, so hint keys can never shadow shell
//!   typing. The arming chord itself must be a modifier chord registered in
//!   the existing keymap (never a bare letter); that registration lives
//!   outside this crate on purpose.
//! * **Generic target API** (008 section 7): any provider registers
//!   [`HintTarget`] values. Seed providers cover
//!   [`CommandBlock`](crate::blocks::CommandBlock) (bridged from #390) and
//!   Panel/View leaves (by raw id, no UI dependency); links/search results
//!   are later providers and are explicitly absent here.
//! * **Single annotation layer** (008 section 9): one ephemeral
//!   [`HintBatch`] carries every label through the compositor as **one**
//!   presentation layer, never one overlay per label.
//!
//! # Overlay-bound bypass rationale (008 section 9, `4+1` bound intact)
//!
//! `bitty-ui::panel::OverlayManager` enforces `MAX_OVERLAYS_PER_WINDOW = 4`
//! plus one modal (`4+1`). A screen of hints can hold up to
//! [`HINT_TARGET_MAX`] labels, so per-label overlays would either blow the
//! bound or force it to 256. Neither happens: [`HintBatch`] is **not** an
//! overlay. It is one ephemeral annotation layer (same category as the
//! [`FoldState`](crate::blocks::FoldState) projection): the compositor
//! paints its glyphs in a single pass and [`HintBatch::overlay_cost`]
//! returns `0`. The `4+1` bound, [`OverlayManager`](https://github.com/bitty-terminal/bitty)
//! state, and every overlay id/generation are therefore untouched by
//! construction — this crate does not even depend on `bitty-ui`, so no code
//! path here can allocate, dismiss, or reorder an overlay. The bound is
//! bypassed only in the sense that hint chrome does not consume it; the
//! bound itself is never widened, narrowed, or skipped.
//!
//! # Terminal Truth (hint chrome is presentation-only)
//!
//! Nothing here mutates [`State`](bitty_term_state::State): no grid write,
//! no scrollback push/clear/resize, no zone/cwd edit. Labels live in
//! [`HintBatch`]; fold side effects land in the caller's
//! [`FoldState`](crate::blocks::FoldState); [`DispatchOutcome::CopyRequested`]
//! carries only a [`TargetId`] so the caller resolves bytes from truth
//! through its own clipboard policy. Copy-all, search, agent history,
//! replay, snapshots, and IPC grid reads therefore never observe hint
//! chrome; proof tests pin `state_hash`, scrollback length, snapshot cells,
//! and search results across the full hint flow.
//!
//! # Bounds (threat T-01)
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`HintRegistry`] targets | [`HINT_TARGET_MAX`] (256) | [`HintRegistry::register`] fails closed (`None`), no eviction |
//! | [`HintBatch`] labels | [`HINT_TARGET_MAX`] (256) | deterministic shed (sorted tail dropped), [`HintBatch::shed`] reports the count |
//! | [`HintBatch`] label text | [`HINT_TEXT_MAX_BYTES`] (8 KiB) | allocation stops at the budget, remainder shed |
//! | parsed label input | [`HINT_LABEL_MAX_CHARS`] (8 chars) | [`parse_hint_chord`] fails closed |
//! | conflict report | 6 entries | at most one per operator key |
//!
//! Fail-closed: with no providers the registry is empty, the batch is empty,
//! [`dispatch`] returns [`DispatchError::UnknownLabel`], and an unarmed
//! [`HintSession::feed`] returns [`HintFeedError::NotArmed`]. No I/O, no
//! wall-clock, no randomness, no unsafe.

#![forbid(unsafe_code)]

use bitty_term_state::State;

use crate::blocks::{CommandBlock, FoldState, blocks};

// ---------------------------------------------------------------------------
// Bounds and alphabets
// ---------------------------------------------------------------------------

/// Maximum hint targets per registry and labels per batch (008 section 9).
///
/// Matches [`COMMAND_BLOCK_MAX`](crate::blocks::COMMAND_BLOCK_MAX) so a full
/// command history always fits the hint budget with room for Panel/View
/// leaves under the same cap.
pub const HINT_TARGET_MAX: usize = 256;

/// Maximum total label text per batch in bytes (008 section 9: 8 KiB).
///
/// Reachable batches stay far below this (256 targets need at most
/// `26 * 1 + 230 * 2 = 486` bytes under the overflow scheme); the budget is
/// enforced anyway so a future alphabet change cannot grow a batch
/// unboundedly.
pub const HINT_TEXT_MAX_BYTES: usize = 8 * 1024;

/// Maximum label length accepted by [`parse_hint_chord`], in chars.
///
/// Built labels never exceed 2 chars while targets stay within
/// [`HINT_TARGET_MAX`]; the parse bound stays generous so input validation
/// cannot reject allocator output, while still bounding caller buffers.
pub const HINT_LABEL_MAX_CHARS: usize = 8;

/// Label alphabet, re-derived for Bitty (NOT copied from the flash.nvim
/// reference): home row first (`a s d f j k l g h`, strongest fingers on a
/// terminal keyboard), then the top row, then the bottom row. Every ASCII
/// lowercase letter appears exactly once, so single-char labels cover the
/// first 26 targets on the easiest keys and multi-char overflow stays
/// mnemonic-free but deterministic.
pub const HINT_LABEL_ALPHABET: &str = "asdfjklghqwertyuiopzxcvbnm";

/// Operator keys accepted by [`HintOperator::from_key`].
///
/// Vim-mnemonic single letters (`j` jump/motion, `z` fold, `p` panel focus,
/// `y` yank/copy, `e` expand, `c` collapse). Consumed only while a
/// [`HintSession`] is armed; see [`check_operator_conflicts`].
pub const HINT_OPERATOR_KEYS: [char; 6] = ['j', 'z', 'p', 'y', 'e', 'c'];

/// Alphabet length; kept as a const so the allocator never hard-codes 26.
const ALPHABET_LEN: usize = 26;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// What kind of UI object a hint target addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HintKind {
    /// A [`CommandBlock`](crate::blocks::CommandBlock) from #390.
    CommandBlock,
    /// A panel leaf, addressed by raw `PanelId` value (no `bitty-ui` dep).
    Panel,
    /// A view leaf, addressed by raw `ViewId` value (no `bitty-ui` dep).
    View,
}

impl HintKind {
    /// Deterministic sort rank: commands first (oldest first by anchor),
    /// then panels, then views. Labels therefore depend only on the target
    /// set, never on registration order.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::CommandBlock => 0,
            Self::Panel => 1,
            Self::View => 2,
        }
    }
}

/// Stable anchor of one hint target: ordinal-anchored for commands (survives
/// resize/reflow/scroll like [`CommandBlock`](crate::blocks::CommandBlock)),
/// id-anchored for Panel/View leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HintAnchor {
    /// Anchored on [`CommandId`](crate::blocks::CommandId) (an OSC 133
    /// ordinal, never a grid row).
    Command(crate::blocks::CommandId),
    /// Raw `PanelId` value; resolved to a panel by the caller.
    Panel(u64),
    /// Raw `ViewId` value; resolved to a view by the caller.
    View(u64),
}

impl HintAnchor {
    /// The [`HintKind`] this anchor addresses.
    #[must_use]
    pub const fn kind(self) -> HintKind {
        match self {
            Self::Command(_) => HintKind::CommandBlock,
            Self::Panel(_) => HintKind::Panel,
            Self::View(_) => HintKind::View,
        }
    }

    /// Raw sort key within a kind: command anchor ordinal, else the leaf id.
    #[must_use]
    pub const fn sort_key(self) -> u64 {
        match self {
            // `CommandId::get` is not const; the field is public, so read it
            // directly to keep this usable in const contexts.
            Self::Command(id) => id.0,
            Self::Panel(raw) | Self::View(raw) => raw,
        }
    }

    /// The command id when this anchor addresses a command block.
    #[must_use]
    pub const fn command_id(self) -> Option<crate::blocks::CommandId> {
        match self {
            Self::Command(id) => Some(id),
            Self::Panel(_) | Self::View(_) => None,
        }
    }
}

/// Verbs applicable to a target: `Action(Target)` (008 section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HintAction {
    /// Focus a Panel/View leaf (commands are not focusable: unsupported).
    Focus,
    /// Jump to a target; for commands also reveals (unfolds) the block.
    Jump,
    /// Flip a command block's fold state (leaves: unsupported).
    ToggleFold,
    /// Ensure a command block is unfolded, idempotent (leaves: unsupported).
    Expand,
    /// Ensure a command block is folded, idempotent (leaves: unsupported).
    Collapse,
    /// Request the target's truth bytes via [`DispatchOutcome::CopyRequested`]
    /// (hint chrome itself is never copied).
    Copy,
}

/// Fixed-size action set for one target (bitset, no allocation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HintActions(u8);

impl HintActions {
    /// No actions.
    pub const EMPTY: Self = Self(0);
    /// [`HintAction::Focus`].
    pub const FOCUS: Self = Self(1 << 0);
    /// [`HintAction::Jump`].
    pub const JUMP: Self = Self(1 << 1);
    /// [`HintAction::ToggleFold`].
    pub const TOGGLE_FOLD: Self = Self(1 << 2);
    /// [`HintAction::Expand`].
    pub const EXPAND: Self = Self(1 << 3);
    /// [`HintAction::Collapse`].
    pub const COLLAPSE: Self = Self(1 << 4);
    /// [`HintAction::Copy`].
    pub const COPY: Self = Self(1 << 5);

    /// Bit position of one action.
    #[must_use]
    pub const fn bit(action: HintAction) -> u8 {
        match action {
            HintAction::Focus => 1 << 0,
            HintAction::Jump => 1 << 1,
            HintAction::ToggleFold => 1 << 2,
            HintAction::Expand => 1 << 3,
            HintAction::Collapse => 1 << 4,
            HintAction::Copy => 1 << 5,
        }
    }

    /// Whether `action` is in the set.
    #[must_use]
    pub const fn contains(self, action: HintAction) -> bool {
        self.0 & Self::bit(action) != 0
    }

    /// Adds `action` to the set.
    pub const fn insert(&mut self, action: HintAction) {
        self.0 |= Self::bit(action);
    }

    /// Number of actions in the set (at most 6).
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Whether the set holds no actions.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Iterates the set in [`HintAction`] declaration order (no allocation).
    pub fn iter(self) -> impl Iterator<Item = HintAction> {
        const ALL: [HintAction; 6] = [
            HintAction::Focus,
            HintAction::Jump,
            HintAction::ToggleFold,
            HintAction::Expand,
            HintAction::Collapse,
            HintAction::Copy,
        ];
        ALL.into_iter().filter(move |a| self.contains(*a))
    }

    /// Default actions per target kind (seed-provider contract):
    /// commands support everything but `Focus`; leaves support `Focus`,
    /// `Jump`, and `Copy`.
    #[must_use]
    pub const fn default_for(kind: HintKind) -> Self {
        match kind {
            HintKind::CommandBlock => Self(
                Self::JUMP.0
                    | Self::TOGGLE_FOLD.0
                    | Self::EXPAND.0
                    | Self::COLLAPSE.0
                    | Self::COPY.0,
            ),
            HintKind::Panel | HintKind::View => Self(Self::FOCUS.0 | Self::JUMP.0 | Self::COPY.0),
        }
    }
}

impl std::ops::BitOr for HintActions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Registry-assigned target handle, 1-based in registration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetId(pub u64);

impl TargetId {
    /// Raw handle value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "target:{}", self.0)
    }
}

/// Opaque caller scope token (window id, workspace generation, ...).
///
/// The registry never interprets it; it is carried on [`HintTarget`] and
/// [`HintLabel`] so the caller can route a batch spanning scopes. Sorting
/// includes the scope so labels stay deterministic for multi-scope sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct HintScope(pub u64);

/// One keyboard-addressable UI object (008 section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HintTarget {
    /// Registry-assigned handle.
    pub id: TargetId,
    /// Object kind (always equals `anchor.kind()`; enforced at registration).
    pub kind: HintKind,
    /// Stable anchor: ordinal for commands, leaf id for panels/views.
    pub anchor: HintAnchor,
    /// Caller scope token.
    pub scope: HintScope,
    /// Verbs this target accepts.
    pub actions: HintActions,
}

/// Ephemeral per-collection target registry (008 section 7: providers,
/// allocator, overlay renderer, and dispatcher stay separate stages).
///
/// A registry is built fresh per hint collection (per generation): providers
/// register, the allocator reads, the batch outlives it. Bounded at
/// [`HINT_TARGET_MAX`]; [`register`](Self::register) fails closed past the
/// cap or on duplicate `(kind, anchor)`.
#[derive(Debug, Clone, Default)]
pub struct HintRegistry {
    targets: Vec<HintTarget>,
    next_id: u64,
}

impl HintRegistry {
    /// Empty registry; first assigned id is `target:1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            targets: Vec::new(),
            next_id: 1,
        }
    }

    /// Registers one target, assigning its [`TargetId`].
    ///
    /// Returns `None` (fail-closed, registry untouched) when full, when an
    /// identical `(kind, anchor)` is already present, when
    /// `kind != anchor.kind()`, or when `actions` is empty. Otherwise
    /// returns the assigned id.
    pub fn register(
        &mut self,
        kind: HintKind,
        anchor: HintAnchor,
        scope: HintScope,
        actions: HintActions,
    ) -> Option<TargetId> {
        if kind != anchor.kind() || actions.is_empty() {
            return None;
        }
        if self.targets.len() >= HINT_TARGET_MAX {
            return None;
        }
        if self
            .targets
            .iter()
            .any(|t| t.kind == kind && t.anchor == anchor)
        {
            return None;
        }
        let id = TargetId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.targets.push(HintTarget {
            id,
            kind,
            anchor,
            scope,
            actions,
        });
        Some(id)
    }

    /// All registered targets in registration order.
    #[must_use]
    pub fn targets(&self) -> &[HintTarget] {
        &self.targets
    }

    /// Number of registered targets (at most [`HINT_TARGET_MAX`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Resolves one target by handle.
    #[must_use]
    pub fn get(&self, id: TargetId) -> Option<&HintTarget> {
        self.targets.iter().find(|t| t.id == id)
    }

    /// Targets accepting `action`, in registration order.
    #[must_use]
    pub fn targets_with(&self, action: HintAction) -> Vec<&HintTarget> {
        self.targets
            .iter()
            .filter(|t| t.actions.contains(action))
            .collect()
    }

    /// Clears the registry (ids are not reused within this registry).
    pub fn clear(&mut self) {
        self.targets.clear();
    }
}

// ---------------------------------------------------------------------------
// Seed providers (P3 scope: CommandBlock + Panel/View leaves)
// ---------------------------------------------------------------------------

/// Registers one target per [`CommandBlock`] (newest-retained, at most
/// [`HINT_TARGET_MAX`]) with [`HintActions::default_for`] command verbs.
///
/// Pure read of `State` via [`blocks`]; never mutates terminal truth.
/// Returns the number of targets admitted (registry-full shedding fails
/// closed and is simply not counted). Links and search results are later
/// providers and are deliberately not collected here.
pub fn collect_command_targets(
    registry: &mut HintRegistry,
    state: &State,
    scope: HintScope,
) -> usize {
    let mut admitted = 0usize;
    for block in blocks(state) {
        let CommandBlock { id, .. } = block;
        if registry
            .register(
                HintKind::CommandBlock,
                HintAnchor::Command(id),
                scope,
                HintActions::default_for(HintKind::CommandBlock),
            )
            .is_some()
        {
            admitted += 1;
        }
    }
    admitted
}

/// Registers Panel leaves by raw `PanelId` value with leaf default verbs.
///
/// Takes raw ids (not `bitty-ui` handles) so this crate gains no new
/// dependency; the caller resolves ids back to panels at dispatch time.
/// Duplicate ids in `panels` are rejected by registry dedup and not counted.
pub fn collect_panel_targets(
    registry: &mut HintRegistry,
    panels: &[u64],
    scope: HintScope,
) -> usize {
    let mut admitted = 0usize;
    for raw in panels.iter().copied() {
        if registry
            .register(
                HintKind::Panel,
                HintAnchor::Panel(raw),
                scope,
                HintActions::default_for(HintKind::Panel),
            )
            .is_some()
        {
            admitted += 1;
        }
    }
    admitted
}

/// Registers View leaves by raw `ViewId` value with leaf default verbs.
///
/// Same raw-id contract as [`collect_panel_targets`].
pub fn collect_view_targets(registry: &mut HintRegistry, views: &[u64], scope: HintScope) -> usize {
    let mut admitted = 0usize;
    for raw in views.iter().copied() {
        if registry
            .register(
                HintKind::View,
                HintAnchor::View(raw),
                scope,
                HintActions::default_for(HintKind::View),
            )
            .is_some()
        {
            admitted += 1;
        }
    }
    admitted
}

// ---------------------------------------------------------------------------
// Label allocator
// ---------------------------------------------------------------------------

/// Computes the label for a rank index: bijective base-26 over
/// [`HINT_LABEL_ALPHABET`] (`a..m`-in-alphabet-order, then `aa`, `ab`, ...).
///
/// Deterministic and total for any index; the allocator only ever passes
/// indices below [`HINT_TARGET_MAX`] (at most 2 chars there). Re-derived for
/// Bitty — the home-row-first alphabet order is what makes short labels land
/// on easy keys, not any property of the flash.nvim reference.
#[must_use]
pub fn label_for_index(index: usize) -> String {
    let alpha = HINT_LABEL_ALPHABET.as_bytes();
    debug_assert_eq!(alpha.len(), ALPHABET_LEN);
    if index < ALPHABET_LEN {
        return String::from(alpha[index] as char);
    }
    // Bijective base-26 (1-based digits, no zero): index 26 -> "aa".
    let mut digits: Vec<char> = Vec::new();
    let mut n = index.saturating_add(1);
    while n > 0 {
        n = n.saturating_sub(1);
        digits.push(alpha[n % ALPHABET_LEN] as char);
        n /= ALPHABET_LEN;
    }
    digits.iter().rev().collect()
}

/// One allocated label: the compositor paints `label` next to the target's
/// anchor as part of the single [`HintBatch`] annotation layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HintLabel {
    /// Target this label selects.
    pub target: TargetId,
    /// Object kind (copied so dispatch needs no registry lookup).
    pub kind: HintKind,
    /// Stable anchor (copied for the same reason).
    pub anchor: HintAnchor,
    /// Caller scope token (copied for cross-scope routing).
    pub scope: HintScope,
    /// Verbs this label accepts (copied for the same reason).
    pub actions: HintActions,
    /// Short label text (`a`, `s`, ..., `aa`, ...; ASCII lowercase only).
    pub label: String,
}

/// Allocates labels over `targets`, deterministic per target set.
///
/// Targets are ordered by `(kind rank, anchor key, scope)` — never by
/// registration order — so the same set always yields the same labels, then
/// labeled `a s d f ...` with bijective base-26 overflow. Bounded: at most
/// [`HINT_TARGET_MAX`] labels and [`HINT_TEXT_MAX_BYTES`] total label bytes;
/// excess targets are shed from the sorted tail (newest commands and
/// highest leaf ids first). Use [`HintBatch::build`] to also learn the shed
/// count.
#[must_use]
pub fn allocate_labels(targets: &[HintTarget]) -> Vec<HintLabel> {
    let mut order: Vec<usize> = (0..targets.len()).collect();
    order.sort_by_key(|i| {
        let t = &targets[*i];
        (t.kind.rank(), t.anchor.sort_key(), t.scope.0, t.id.0)
    });
    let mut labels = Vec::new();
    let mut bytes = 0usize;
    for (rank, i) in order.into_iter().enumerate() {
        if rank >= HINT_TARGET_MAX {
            break;
        }
        let label = label_for_index(rank);
        bytes = bytes.saturating_add(label.len());
        if bytes > HINT_TEXT_MAX_BYTES {
            break;
        }
        let t = &targets[i];
        labels.push(HintLabel {
            target: t.id,
            kind: t.kind,
            anchor: t.anchor,
            scope: t.scope,
            actions: t.actions,
            label,
        });
    }
    labels
}

// ---------------------------------------------------------------------------
// HintBatch: the single ephemeral annotation layer
// ---------------------------------------------------------------------------

/// One ephemeral annotation layer through the compositor (008 section 9).
///
/// A batch is built once per hint collection (`generation`) and carries every
/// label as data; the compositor paints them in a single pass as
/// presentation-only glyphs. It is **not** `HINT_TARGET_MAX` overlays (see
/// [`HintBatch::overlay_cost`] and the module-level bypass rationale), never
/// enters scrollback/grid truth, copy paths, search, or IPC grid reads, and
/// dies on [`HintSession::disarm`] (or by simply dropping it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintBatch {
    /// Collection generation this batch was built for.
    pub generation: u64,
    /// Allocated labels in label order (`labels[i]` holds the rank-`i` label).
    pub labels: Vec<HintLabel>,
    /// Targets shed by the [`HINT_TARGET_MAX`] / [`HINT_TEXT_MAX_BYTES`]
    /// caps (sorted tail: newest commands, highest leaf ids first).
    pub shed: usize,
}

impl HintBatch {
    /// Builds the batch for `generation` from a registry snapshot.
    ///
    /// Deterministic: identical registries (as sets) yield identical batches
    /// up to `generation`. Bounded by construction; [`shed`](Self::shed)
    /// reports cap shedding.
    #[must_use]
    pub fn build(generation: u64, registry: &HintRegistry) -> Self {
        let labels = allocate_labels(registry.targets());
        let shed = registry.len().saturating_sub(labels.len());
        Self {
            generation,
            labels,
            shed,
        }
    }

    /// Number of labels (at most [`HINT_TARGET_MAX`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// Whether the batch holds no labels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Resolves an exact label string (case-sensitive; allocator output is
    /// lowercase ASCII, so anything else fails closed with `None`).
    #[must_use]
    pub fn resolve(&self, label: &str) -> Option<&HintLabel> {
        self.labels.iter().find(|l| l.label == label)
    }

    /// Label text for a target handle, if present.
    #[must_use]
    pub fn label_for(&self, id: TargetId) -> Option<&str> {
        self.labels
            .iter()
            .find(|l| l.target == id)
            .map(|l| l.label.as_str())
    }

    /// Total label text bytes (always at most [`HINT_TEXT_MAX_BYTES`]).
    #[must_use]
    pub fn total_text_bytes(&self) -> usize {
        self.labels.iter().map(|l| l.label.len()).sum()
    }

    /// Overlay slots consumed by this batch: always `0`.
    ///
    /// Contract anchor for the `4+1` bound: hint chrome rides the single
    /// annotation layer and never allocates, dismisses, or reorders
    /// `OverlayManager` entries (this crate cannot even name that type).
    #[must_use]
    pub const fn overlay_cost(&self) -> usize {
        0
    }
}

// ---------------------------------------------------------------------------
// Dispatcher: Action(Target)
// ---------------------------------------------------------------------------

/// What dispatching one `(label, action)` pair did.
///
/// Intents that need caller context (`FocusView`, `FocusPanel`, `Jump`,
/// `CopyRequested`) carry ids only — never grid text, never label chrome —
/// so the caller resolves them against truth through its own focus, scroll,
/// and clipboard policies. Fold mutations apply directly to the caller's
/// [`FoldState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchOutcome {
    /// Focus the view leaf (caller resolves the raw `ViewId` value).
    FocusView {
        /// Raw view id from the target anchor.
        view: u64,
    },
    /// Focus the panel leaf (caller resolves the raw `PanelId` value).
    FocusPanel {
        /// Raw panel id from the target anchor.
        panel: u64,
    },
    /// Jump to the target (caller scrolls/reveals it); command targets are
    /// unfolded first so the jump always lands on visible rows.
    Jump {
        /// Jumped-to target.
        target: TargetId,
    },
    /// A command block's fold state flipped.
    FoldToggled {
        /// Toggled block.
        id: crate::blocks::CommandId,
        /// Whether the block is folded now.
        folded: bool,
    },
    /// A command block ensured unfolded (idempotent).
    Expanded {
        /// Expanded block.
        id: crate::blocks::CommandId,
    },
    /// A command block ensured folded (idempotent).
    Collapsed {
        /// Collapsed block.
        id: crate::blocks::CommandId,
    },
    /// Copy requested: the caller must resolve the target's bytes from
    /// terminal truth (never from hint chrome) and route them through its
    /// clipboard policy. Carries the handle only, by construction.
    CopyRequested {
        /// Target whose truth bytes should be copied.
        target: TargetId,
    },
}

/// Why a dispatch failed (all fail-closed: [`FoldState`] untouched, except a
/// successful unfold preceding... no — errors never mutate `fold`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatchError {
    /// No label in this batch matches (stale or mistyped input).
    UnknownLabel,
    /// The target does not accept this action (e.g. `Focus` on a command).
    ActionNotSupported {
        /// Rejected action.
        action: HintAction,
        /// Kind it was applied to.
        kind: HintKind,
    },
    /// `Collapse`/`ToggleFold`-to-folded refused: fold set is at
    /// [`FOLD_MAX`](crate::blocks::FOLD_MAX). Unfolding still works.
    FoldFull {
        /// Block that stays unfolded.
        id: crate::blocks::CommandId,
    },
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownLabel => f.write_str("unknown hint label"),
            Self::ActionNotSupported { action, kind } => {
                write!(f, "{action:?} not supported for {kind:?}")
            }
            Self::FoldFull { id } => write!(f, "fold set full; {id} stays unfolded"),
        }
    }
}

impl std::error::Error for DispatchError {}

/// Dispatches `Action(Target)`: resolves `label` in `batch` and applies
/// `action`, mutating only the caller's [`FoldState`].
///
/// Pure with respect to terminal truth: the only mutation in the system is
/// fold membership (presentation projection, owned by the caller). Errors
/// leave `fold` untouched.
pub fn dispatch(
    batch: &HintBatch,
    fold: &mut FoldState,
    label: &str,
    action: HintAction,
) -> Result<DispatchOutcome, DispatchError> {
    let found = batch.resolve(label).ok_or(DispatchError::UnknownLabel)?;
    if !found.actions.contains(action) {
        return Err(DispatchError::ActionNotSupported {
            action,
            kind: found.kind,
        });
    }
    match (found.kind, action) {
        (HintKind::Panel, HintAction::Focus) => match found.anchor {
            HintAnchor::Panel(raw) => Ok(DispatchOutcome::FocusPanel { panel: raw }),
            _ => Err(DispatchError::ActionNotSupported {
                action,
                kind: found.kind,
            }),
        },
        (HintKind::View, HintAction::Focus) => match found.anchor {
            HintAnchor::View(raw) => Ok(DispatchOutcome::FocusView { view: raw }),
            _ => Err(DispatchError::ActionNotSupported {
                action,
                kind: found.kind,
            }),
        },
        (_, HintAction::Jump) => {
            if let Some(id) = found.anchor.command_id() {
                // Reveal first: jumping to a folded-away block must land on
                // visible rows. Unfolding cannot fail.
                fold.unfold(id);
            }
            Ok(DispatchOutcome::Jump {
                target: found.target,
            })
        }
        (HintKind::CommandBlock, HintAction::ToggleFold) => {
            let Some(id) = found.anchor.command_id() else {
                return Err(DispatchError::ActionNotSupported {
                    action,
                    kind: found.kind,
                });
            };
            if fold.is_folded(id) {
                fold.unfold(id);
                Ok(DispatchOutcome::FoldToggled { id, folded: false })
            } else if fold.fold(id) {
                Ok(DispatchOutcome::FoldToggled { id, folded: true })
            } else {
                Err(DispatchError::FoldFull { id })
            }
        }
        (HintKind::CommandBlock, HintAction::Expand) => {
            let Some(id) = found.anchor.command_id() else {
                return Err(DispatchError::ActionNotSupported {
                    action,
                    kind: found.kind,
                });
            };
            fold.unfold(id);
            Ok(DispatchOutcome::Expanded { id })
        }
        (HintKind::CommandBlock, HintAction::Collapse) => {
            let Some(id) = found.anchor.command_id() else {
                return Err(DispatchError::ActionNotSupported {
                    action,
                    kind: found.kind,
                });
            };
            if fold.is_folded(id) {
                return Ok(DispatchOutcome::Collapsed { id });
            }
            if fold.fold(id) {
                Ok(DispatchOutcome::Collapsed { id })
            } else {
                Err(DispatchError::FoldFull { id })
            }
        }
        (_, HintAction::Copy) => Ok(DispatchOutcome::CopyRequested {
            target: found.target,
        }),
        _ => Err(DispatchError::ActionNotSupported {
            action,
            kind: found.kind,
        }),
    }
}

// ---------------------------------------------------------------------------
// Operator-style chords (Leader sequences without keymap hijack)
// ---------------------------------------------------------------------------

/// Operator half of a hint chord: selects the [`HintAction`], the label
/// selects the target (008 section 6: Vim operator + motion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HintOperator {
    /// `j`: [`HintAction::Jump`].
    Jump,
    /// `z`: [`HintAction::ToggleFold`].
    ToggleFold,
    /// `p`: [`HintAction::Focus`] (panel/view leaves).
    Focus,
    /// `y`: [`HintAction::Copy`] (vim yank mnemonic).
    Copy,
    /// `e`: [`HintAction::Expand`].
    Expand,
    /// `c`: [`HintAction::Collapse`].
    Collapse,
}

impl HintOperator {
    /// Parses one operator key (exact lowercase only, fail-closed).
    #[must_use]
    pub const fn from_key(key: char) -> Option<Self> {
        match key {
            'j' => Some(Self::Jump),
            'z' => Some(Self::ToggleFold),
            'p' => Some(Self::Focus),
            'y' => Some(Self::Copy),
            'e' => Some(Self::Expand),
            'c' => Some(Self::Collapse),
            _ => None,
        }
    }

    /// The key that selects this operator.
    #[must_use]
    pub const fn key(self) -> char {
        match self {
            Self::Jump => 'j',
            Self::ToggleFold => 'z',
            Self::Focus => 'p',
            Self::Copy => 'y',
            Self::Expand => 'e',
            Self::Collapse => 'c',
        }
    }

    /// The action this operator dispatches.
    #[must_use]
    pub const fn action(self) -> HintAction {
        match self {
            Self::Jump => HintAction::Jump,
            Self::ToggleFold => HintAction::ToggleFold,
            Self::Focus => HintAction::Focus,
            Self::Copy => HintAction::Copy,
            Self::Expand => HintAction::Expand,
            Self::Collapse => HintAction::Collapse,
        }
    }

    /// All operator keys (mirrors [`HINT_OPERATOR_KEYS`]).
    #[must_use]
    pub const fn all_keys() -> [char; 6] {
        HINT_OPERATOR_KEYS
    }
}

/// A parsed `operator + label` chord, ready for [`dispatch`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HintChord {
    /// Operator half (selects the action).
    pub operator: HintOperator,
    /// Label half (selects the target; validated allocator-shaped text).
    pub label: String,
}

/// Why chord parsing failed (all fail-closed: nothing is armed or dispatched).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChordError {
    /// First key is not an operator (`HintOperator::from_key` is `None`).
    UnknownOperator {
        /// Rejected key.
        key: char,
    },
    /// Label half is empty.
    EmptyLabel,
    /// Label half exceeds [`HINT_LABEL_MAX_CHARS`] chars.
    LabelTooLong {
        /// Observed char count.
        chars: usize,
    },
    /// Label half contains non-alphabet chars (only [`HINT_LABEL_ALPHABET`]).
    LabelCharset,
}

impl std::fmt::Display for ChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOperator { key } => write!(f, "unknown hint operator '{key}'"),
            Self::EmptyLabel => f.write_str("hint label must not be empty"),
            Self::LabelTooLong { chars } => {
                write!(f, "hint label too long: {chars} chars")
            }
            Self::LabelCharset => f.write_str("hint label outside label alphabet"),
        }
    }
}

impl std::error::Error for ChordError {}

/// Parses an `operator + label` chord (the keys pressed after the Leader).
///
/// Total and allocation-bounded: the label is at most [`HINT_LABEL_MAX_CHARS`]
/// chars of [`HINT_LABEL_ALPHABET`]. Resolving the label against a batch
/// stays the caller's step ([`HintBatch::resolve`] fails closed on stale or
/// foreign labels).
pub fn parse_hint_chord(operator_key: char, label: &str) -> Result<HintChord, ChordError> {
    let operator = HintOperator::from_key(operator_key)
        .ok_or(ChordError::UnknownOperator { key: operator_key })?;
    if label.is_empty() {
        return Err(ChordError::EmptyLabel);
    }
    let chars = label.chars().count();
    if chars > HINT_LABEL_MAX_CHARS {
        return Err(ChordError::LabelTooLong { chars });
    }
    let alpha = HINT_LABEL_ALPHABET.as_bytes();
    if !label.bytes().all(|b| alpha.contains(&b)) {
        return Err(ChordError::LabelCharset);
    }
    Ok(HintChord {
        operator,
        label: label.to_owned(),
    })
}

/// Operator keys that collide with caller-bound bare keys.
///
/// Returned by [`check_operator_conflicts`]; at most one entry per operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorConflict {
    /// Colliding keys, sorted and deduplicated.
    pub keys: Vec<char>,
}

impl std::fmt::Display for OperatorConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hint operators collide with bound keys: ")?;
        for (i, key) in self.keys.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "'{key}'")?;
        }
        Ok(())
    }
}

impl std::error::Error for OperatorConflict {}

/// Fail-closed conflict gate between hint operators and the existing keymap.
///
/// The caller passes every **bare** (modifier-free) single-char key its
/// keymap binds to a chrome action — normally empty, because the keymap
/// schema requires a modifier on single-char chords so shell typing is never
/// stolen. Any overlap with [`HINT_OPERATOR_KEYS`] is an error: arming a
/// [`HintSession`] with colliding operators would shadow those chrome keys
/// (or, worse, shell input routed through them), so the session refuses to
/// arm instead. Comparison is ASCII case-insensitive; label chars need no
/// check because they are consumed only after a valid operator while armed
/// (single-owner rule, documented on [`HintSession`]).
pub fn check_operator_conflicts(bound_bare_keys: &[char]) -> Result<(), OperatorConflict> {
    let mut hits: Vec<char> = HINT_OPERATOR_KEYS
        .iter()
        .copied()
        .filter(|op| {
            bound_bare_keys
                .iter()
                .any(|b| b.to_ascii_lowercase() == *op)
        })
        .collect();
    hits.sort_unstable();
    hits.dedup();
    if hits.is_empty() {
        Ok(())
    } else {
        Err(OperatorConflict { keys: hits })
    }
}

// ---------------------------------------------------------------------------
// Session: armed routing around one batch
// ---------------------------------------------------------------------------

/// Why a session feed failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintFeedError {
    /// [`HintSession::feed`] called while disarmed (keys belong to the shell).
    NotArmed,
    /// Chord half failed to parse; nothing dispatched.
    Chord(ChordError),
    /// Dispatch failed; [`FoldState`] untouched.
    Dispatch(DispatchError),
}

impl std::fmt::Display for HintFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotArmed => f.write_str("hint session is not armed"),
            Self::Chord(e) => write!(f, "hint chord: {e}"),
            Self::Dispatch(e) => write!(f, "hint dispatch: {e}"),
        }
    }
}

impl std::error::Error for HintFeedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotArmed => None,
            Self::Chord(e) => Some(e),
            Self::Dispatch(e) => Some(e),
        }
    }
}

impl From<ChordError> for HintFeedError {
    fn from(value: ChordError) -> Self {
        Self::Chord(value)
    }
}

impl From<DispatchError> for HintFeedError {
    fn from(value: DispatchError) -> Self {
        Self::Dispatch(value)
    }
}

/// Armed routing around one [`HintBatch`]: the single-owner rule made explicit.
///
/// While armed, operator/label keys route to [`feed`](Self::feed) and never
/// to the PTY; while disarmed (the default), every key belongs to the shell
/// and [`feed`](Self::feed) fails closed with [`HintFeedError::NotArmed`].
/// The caller flips between the two on its Leader chord (a modifier chord in
/// the existing keymap) and on `Esc`/dismiss. [`arm`](Self::arm) runs
/// [`check_operator_conflicts`] first so a conflicting keymap can never
/// reach the armed state; [`disarm`](Self::disarm) drops the batch because
/// hint chrome is ephemeral (a new collection builds a new batch).
#[derive(Debug, Clone, Default)]
pub struct HintSession {
    batch: Option<HintBatch>,
    armed: bool,
}

impl HintSession {
    /// Disarmed session with no batch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            batch: None,
            armed: false,
        }
    }

    /// Arms the session on `batch` after the conflict gate passes.
    ///
    /// # Errors
    /// [`OperatorConflict`] when any operator collides with
    /// `bound_bare_keys`; the session stays disarmed and keeps no batch.
    pub fn arm(
        &mut self,
        batch: HintBatch,
        bound_bare_keys: &[char],
    ) -> Result<(), OperatorConflict> {
        check_operator_conflicts(bound_bare_keys)?;
        self.batch = Some(batch);
        self.armed = true;
        Ok(())
    }

    /// Feeds one `operator + label` chord through parse + dispatch.
    ///
    /// # Errors
    /// [`HintFeedError::NotArmed`] while disarmed; parse/dispatch errors
    /// otherwise (fold untouched on any error).
    pub fn feed(
        &self,
        fold: &mut FoldState,
        operator_key: char,
        label: &str,
    ) -> Result<DispatchOutcome, HintFeedError> {
        if !self.armed {
            return Err(HintFeedError::NotArmed);
        }
        let Some(batch) = self.batch.as_ref() else {
            return Err(HintFeedError::NotArmed);
        };
        let chord = parse_hint_chord(operator_key, label)?;
        dispatch(batch, fold, &chord.label, chord.operator.action()).map_err(HintFeedError::from)
    }

    /// Disarms and drops the batch (hint chrome is ephemeral).
    pub fn disarm(&mut self) {
        self.batch = None;
        self.armed = false;
    }

    /// Whether operator/label keys currently route to [`feed`](Self::feed).
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// The armed batch, if any.
    #[must_use]
    pub fn batch(&self) -> Option<&HintBatch> {
        self.batch.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::CommandId;
    use bitty_term_state::{State, TerminalAction, ZoneKind};
    use bitty_vt::{ControlChar, GraphemeCell};

    const SCOPE: HintScope = HintScope(7);

    fn mark(state: &mut State, kind: ZoneKind) {
        state.apply(&TerminalAction::OscPromptMark {
            kind,
            exit_code: None,
        });
    }

    fn mark_exit(state: &mut State, code: i32) {
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputEnd,
            exit_code: Some(code),
        });
    }

    fn full_cycle(state: &mut State, exit: i32) {
        mark(state, ZoneKind::PromptStart);
        mark(state, ZoneKind::InputStart);
        mark(state, ZoneKind::OutputStart);
        mark_exit(state, exit);
    }

    fn print(state: &mut State, s: &str) {
        for ch in s.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }

    fn lf(state: &mut State) {
        state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }

    fn line(state: &mut State, s: &str) {
        print(state, s);
        lf(state);
    }

    fn two_command_state() -> State {
        let mut state = State::new();
        full_cycle(&mut state, 0);
        full_cycle(&mut state, 3);
        for _ in 0..10 {
            line(&mut state, "output filler");
        }
        state
    }

    // -- registry ----------------------------------------------------------
    #[test]
    fn registry_register_dedup_and_bounded() {
        let mut registry = HintRegistry::new();
        assert!(registry.is_empty());
        let id = registry
            .register(
                HintKind::View,
                HintAnchor::View(3),
                SCOPE,
                HintActions::FOCUS | HintActions::JUMP,
            )
            .expect("first registration admits");
        assert_eq!(id, TargetId(1));
        assert_eq!(registry.len(), 1);
        // Duplicate (kind, anchor): fail closed, untouched.
        assert!(
            registry
                .register(
                    HintKind::View,
                    HintAnchor::View(3),
                    SCOPE,
                    HintActions::JUMP
                )
                .is_none()
        );
        assert_eq!(registry.len(), 1);
        // Same raw id under a different kind is a different target.
        assert!(
            registry
                .register(
                    HintKind::Panel,
                    HintAnchor::Panel(3),
                    SCOPE,
                    HintActions::default_for(HintKind::Panel),
                )
                .is_some()
        );
        // kind/anchor mismatch: fail closed.
        assert!(
            registry
                .register(
                    HintKind::Panel,
                    HintAnchor::View(9),
                    SCOPE,
                    HintActions::JUMP
                )
                .is_none()
        );
        // Empty action set: fail closed.
        assert!(
            registry
                .register(
                    HintKind::View,
                    HintAnchor::View(9),
                    SCOPE,
                    HintActions::EMPTY
                )
                .is_none()
        );
        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.get(id).expect("get resolves").anchor,
            HintAnchor::View(3)
        );
        assert!(registry.get(TargetId(999)).is_none());
        // Fill to the cap: the 257th registration fails closed, no eviction.
        for raw in 100..(100 + HINT_TARGET_MAX as u64) {
            let _ = registry.register(
                HintKind::View,
                HintAnchor::View(raw),
                SCOPE,
                HintActions::JUMP,
            );
        }
        assert_eq!(registry.len(), HINT_TARGET_MAX);
        assert!(
            registry
                .register(
                    HintKind::View,
                    HintAnchor::View(u64::MAX),
                    SCOPE,
                    HintActions::JUMP
                )
                .is_none()
        );
        assert_eq!(registry.len(), HINT_TARGET_MAX);
    }

    #[test]
    fn seed_providers_command_panel_view() {
        let state = two_command_state();
        let mut registry = HintRegistry::new();
        let commands = collect_command_targets(&mut registry, &state, SCOPE);
        assert_eq!(commands, 2);
        let panels = collect_panel_targets(&mut registry, &[11, 22, 11], SCOPE);
        assert_eq!(panels, 2, "duplicate panel id shed by dedup");
        let views = collect_view_targets(&mut registry, &[5], SCOPE);
        assert_eq!(views, 1);
        assert_eq!(registry.len(), 5);
        // Default verbs per kind.
        let kinds: Vec<(HintKind, HintActions)> = registry
            .targets()
            .iter()
            .map(|t| (t.kind, t.actions))
            .collect();
        assert_eq!(kinds[0].0, HintKind::CommandBlock);
        assert!(!kinds[0].1.contains(HintAction::Focus));
        assert!(kinds[0].1.contains(HintAction::ToggleFold));
        assert!(kinds[0].1.contains(HintAction::Copy));
        for (kind, actions) in &kinds[2..] {
            assert!(kind == &HintKind::Panel || kind == &HintKind::View);
            assert!(actions.contains(HintAction::Focus));
            assert!(!actions.contains(HintAction::ToggleFold));
        }
        // Action filter reads back correctly.
        assert_eq!(registry.targets_with(HintAction::Focus).len(), 3);
        assert_eq!(registry.targets_with(HintAction::ToggleFold).len(), 2);
        // Empty state collects nothing, fail-closed.
        let mut empty = HintRegistry::new();
        assert_eq!(collect_command_targets(&mut empty, &State::new(), SCOPE), 0);
        assert!(empty.is_empty());
    }

    // -- allocator ---------------------------------------------------------

    #[test]
    fn label_scheme_spot_checks() {
        let alpha: Vec<char> = HINT_LABEL_ALPHABET.chars().collect();
        assert_eq!(alpha.len(), 26, "alphabet covers a-z exactly once");
        let mut sorted = alpha.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 26);
        assert_eq!(label_for_index(0), "a");
        assert_eq!(label_for_index(1), "s");
        assert_eq!(label_for_index(25), "m");
        assert_eq!(label_for_index(26), "aa");
        assert_eq!(label_for_index(27), "as");
        // Uniqueness over the reachable range.
        let all: Vec<String> = (0..HINT_TARGET_MAX).map(label_for_index).collect();
        let mut dedup = all.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(all.len(), dedup.len(), "labels unique over reachable range");
        assert!(all.iter().take(26).all(|l| l.len() == 1));
    }

    #[test]
    fn allocation_deterministic_across_registration_order() {
        let state = two_command_state();
        let mut forward = HintRegistry::new();
        collect_command_targets(&mut forward, &state, SCOPE);
        collect_panel_targets(&mut forward, &[11, 22], SCOPE);
        collect_view_targets(&mut forward, &[5], SCOPE);
        let mut backward = HintRegistry::new();
        collect_view_targets(&mut backward, &[5], SCOPE);
        collect_panel_targets(&mut backward, &[22, 11], SCOPE);
        collect_command_targets(&mut backward, &state, SCOPE);
        let label_map = |registry: &HintRegistry| {
            let batch = HintBatch::build(1, registry);
            let mut pairs: Vec<((u8, u64), String)> = batch
                .labels
                .iter()
                .map(|l| ((l.kind.rank(), l.anchor.sort_key()), l.label.clone()))
                .collect();
            pairs.sort();
            pairs
        };
        assert_eq!(label_map(&forward), label_map(&backward));
        // Labels follow sorted order: first command owns "a".
        let batch = HintBatch::build(9, &forward);
        assert_eq!(batch.generation, 9);
        assert_eq!(batch.labels[0].kind, HintKind::CommandBlock);
        assert_eq!(batch.labels[0].label, "a");
        // Same set, new generation: same labels.
        let again = HintBatch::build(10, &forward);
        let labels: Vec<&str> = batch.labels.iter().map(|l| l.label.as_str()).collect();
        let relabeled: Vec<&str> = again.labels.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, relabeled);
    }

    #[test]
    fn batch_cap_shedding_bounded_text() {
        let mut registry = HintRegistry::new();
        let views: Vec<u64> = (1..=300).collect();
        let admitted = collect_view_targets(&mut registry, &views, SCOPE);
        assert_eq!(admitted, HINT_TARGET_MAX, "registry cap sheds the tail");
        let batch = HintBatch::build(3, &registry);
        assert_eq!(batch.len(), HINT_TARGET_MAX);
        // Registry already capped, so build sheds nothing here...
        assert_eq!(batch.shed, 0);
        // ...but oversized target sets shed deterministically at build.
        let mut big = HintRegistry::new();
        for raw in 1..=HINT_TARGET_MAX as u64 {
            let _ = big.register(
                HintKind::View,
                HintAnchor::View(raw),
                SCOPE,
                HintActions::JUMP,
            );
        }
        // Simulate a 300-target set by checking the allocator directly.
        let mut targets: Vec<HintTarget> = big.targets().to_vec();
        for raw in 1000..1044 {
            targets.push(HintTarget {
                id: TargetId(raw),
                kind: HintKind::View,
                anchor: HintAnchor::View(raw),
                scope: SCOPE,
                actions: HintActions::JUMP,
            });
        }
        // Highest leaf ids shed first (sorted-tail policy).
        let labels = allocate_labels(&targets);
        assert_eq!(labels.len(), HINT_TARGET_MAX);
        let kept_max = labels
            .iter()
            .map(|l| l.anchor.sort_key())
            .max()
            .expect("labels");
        assert_eq!(kept_max, HINT_TARGET_MAX as u64);
        let shed = targets.len() - labels.len();
        assert_eq!(shed, 300 - HINT_TARGET_MAX);
        assert!(
            batch.total_text_bytes() <= HINT_TEXT_MAX_BYTES,
            "8 KiB budget holds: {}",
            batch.total_text_bytes()
        );
        // Labels unique within the batch.
        let mut seen: Vec<&str> = batch.labels.iter().map(|l| l.label.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), batch.len());
    }

    // -- dispatch ----------------------------------------------------------

    fn armed_session(state: &State) -> (HintSession, HintBatch, HintRegistry) {
        let mut registry = HintRegistry::new();
        collect_command_targets(&mut registry, state, SCOPE);
        collect_panel_targets(&mut registry, &[11], SCOPE);
        collect_view_targets(&mut registry, &[5], SCOPE);
        let batch = HintBatch::build(1, &registry);
        // Layout: 2 commands ("a","s"), panel 11 ("d"... kinds sort Panel
        // after commands), view 5. Resolve dynamically to stay robust.
        let mut session = HintSession::new();
        session.arm(batch.clone(), &[]).expect("no conflicts");
        (session, batch, registry)
    }

    #[test]
    fn dispatch_correctness_per_action() {
        let state = two_command_state();
        let (session, batch, _) = armed_session(&state);
        let mut fold = FoldState::new();
        let label_of = |kind: HintKind| {
            batch
                .labels
                .iter()
                .find(|l| l.kind == kind)
                .expect("kind present")
                .label
                .clone()
        };
        let cmd_label = label_of(HintKind::CommandBlock);
        let panel_label = label_of(HintKind::Panel);
        let view_label = label_of(HintKind::View);

        // ToggleFold flips both ways.
        let toggled = session.feed(&mut fold, 'z', &cmd_label).expect("toggle");
        let block_id = match toggled {
            DispatchOutcome::FoldToggled { id, folded } => {
                assert!(folded);
                id
            }
            other => panic!("expected FoldToggled, got {other:?}"),
        };
        assert!(fold.is_folded(block_id));
        // Jump reveals the folded block as a side effect.
        let jumped = session.feed(&mut fold, 'j', &cmd_label).expect("jump");
        assert!(matches!(jumped, DispatchOutcome::Jump { .. }));
        assert!(!fold.is_folded(block_id), "jump unfolds its target");
        // Collapse is idempotent; Expand restores.
        assert!(matches!(
            session.feed(&mut fold, 'c', &cmd_label).expect("collapse"),
            DispatchOutcome::Collapsed { .. }
        ));
        assert!(fold.is_folded(block_id));
        assert!(matches!(
            session
                .feed(&mut fold, 'c', &cmd_label)
                .expect("re-collapse"),
            DispatchOutcome::Collapsed { .. }
        ));
        assert!(matches!(
            session.feed(&mut fold, 'e', &cmd_label).expect("expand"),
            DispatchOutcome::Expanded { .. }
        ));
        assert!(!fold.is_folded(block_id));
        // Focus resolves raw leaf ids for panel/view, rejects commands.
        assert_eq!(
            session
                .feed(&mut fold, 'p', &panel_label)
                .expect("panel focus"),
            DispatchOutcome::FocusPanel { panel: 11 }
        );
        assert_eq!(
            session
                .feed(&mut fold, 'p', &view_label)
                .expect("view focus"),
            DispatchOutcome::FocusView { view: 5 }
        );
        assert_eq!(
            session.feed(&mut fold, 'p', &cmd_label),
            Err(HintFeedError::Dispatch(DispatchError::ActionNotSupported {
                action: HintAction::Focus,
                kind: HintKind::CommandBlock,
            }))
        );
        // Copy carries the handle only — no text, no chrome. The outcome
        // round-trips to the fed label through the batch, proving the
        // request references the target (whose bytes the caller resolves
        // from truth) rather than any label chrome.
        let copied = session.feed(&mut fold, 'y', &cmd_label).expect("copy");
        match copied {
            DispatchOutcome::CopyRequested { target } => {
                assert_eq!(batch.label_for(target), Some(cmd_label.as_str()));
            }
            other => panic!("expected CopyRequested, got {other:?}"),
        }
        // Fold verbs on leaves are unsupported, fail-closed.
        assert!(matches!(
            session.feed(&mut fold, 'z', &panel_label),
            Err(HintFeedError::Dispatch(
                DispatchError::ActionNotSupported { .. }
            ))
        ));
        // Unknown label fails closed.
        assert_eq!(
            session.feed(&mut fold, 'j', "qqq"),
            Err(HintFeedError::Dispatch(DispatchError::UnknownLabel))
        );
    }

    #[test]
    fn dispatch_fold_full_fails_closed() {
        let state = two_command_state();
        let (_session, batch, _) = armed_session(&state);
        let mut fold = FoldState::new();
        // Saturate the fold set with foreign ids.
        for i in 0..crate::blocks::FOLD_MAX as u64 {
            assert!(fold.fold(CommandId(1_000_000 + i)));
        }
        let cmd_label = batch
            .labels
            .iter()
            .find(|l| l.kind == HintKind::CommandBlock)
            .expect("command label")
            .label
            .clone();
        let before = fold.folded_count();
        assert!(matches!(
            dispatch(&batch, &mut fold, &cmd_label, HintAction::Collapse),
            Err(DispatchError::FoldFull { .. })
        ));
        assert_eq!(fold.folded_count(), before, "failed fold mutates nothing");
        // Unfolding still works at cap.
        fold.unfold(CommandId(1_000_000));
        assert!(matches!(
            dispatch(&batch, &mut fold, &cmd_label, HintAction::Expand),
            Ok(DispatchOutcome::Expanded { .. })
        ));
    }

    // -- overlay bound + truth ------------------------------------------------

    #[test]
    fn hint_batch_consumes_no_overlay_budget() {
        // Contract anchor: the batch is one annotation layer, zero overlays.
        // (This crate has no `bitty-ui` dependency, so no code path here can
        // even name the overlay manager; the bound can only be observed as
        // untouched from the caller's side.)
        let mut registry = HintRegistry::new();
        let views: Vec<u64> = (1..=HINT_TARGET_MAX as u64).collect();
        collect_view_targets(&mut registry, &views, SCOPE);
        let batch = HintBatch::build(1, &registry);
        assert_eq!(batch.len(), HINT_TARGET_MAX);
        assert_eq!(batch.overlay_cost(), 0);
        // Batches share no mutable state: two builds are independent values.
        let again = HintBatch::build(2, &registry);
        assert_eq!(batch.labels, again.labels);
        assert_ne!(batch.generation, again.generation);
    }

    #[test]
    fn no_hint_chrome_leaks_into_truth_copy_search_ipc() {
        let mut state = two_command_state();
        print(&mut state, "needle-secret-output");
        lf(&mut state);
        let hash_before = state.state_hash();
        let sb_before = state.scrollback_len();
        let snap_before = state.snapshot();
        let search_before =
            state.search("needle", bitty_term_state::search::SearchOptions::default());
        assert!(!search_before.is_empty(), "fixture must be searchable");

        // Full hint flow: collect, allocate, arm, feed every verb. Extra view
        // leaves force multi-char overflow labels so the grid scan below is
        // probative (not vacuously true on single-char labels).
        let mut registry = HintRegistry::new();
        collect_command_targets(&mut registry, &state, SCOPE);
        collect_panel_targets(&mut registry, &[11], SCOPE);
        let extra_views: Vec<u64> = (5..45).collect();
        collect_view_targets(&mut registry, &extra_views, SCOPE);
        let batch = HintBatch::build(4, &registry);
        assert!(batch.labels.iter().any(|l| l.label.len() > 1));
        let mut session = HintSession::new();
        session.arm(batch.clone(), &[]).expect("arm");
        let mut fold = FoldState::new();
        for label in batch
            .labels
            .iter()
            .map(|l| l.label.clone())
            .collect::<Vec<_>>()
        {
            for op in ['j', 'y'] {
                let _ = session.feed(&mut fold, op, &label);
            }
        }
        for label in batch
            .labels
            .iter()
            .map(|l| l.label.clone())
            .collect::<Vec<_>>()
        {
            let _ = session.feed(&mut fold, 'z', &label);
            let _ = session.feed(&mut fold, 'e', &label);
        }
        session.disarm();
        assert!(!session.is_armed());

        // Terminal truth is byte-identical: grid, scrollback, search, and
        // therefore every copy/IPC grid read built on them.
        assert_eq!(
            state.state_hash(),
            hash_before,
            "hint flow must not touch truth"
        );
        assert_eq!(state.scrollback_len(), sb_before);
        assert_eq!(state.snapshot().cells, snap_before.cells);
        assert_eq!(
            state.search("needle", bitty_term_state::search::SearchOptions::default()),
            search_before,
            "search reads truth, never hint chrome"
        );
        // No multi-char label appears as contiguous grid text: scan every
        // snapshot row for every multi-char label in the batch. (Single-char
        // labels would trivially match content, so only multi-char labels
        // are probative — and the hash equality above already proves the
        // grid is byte-identical.)
        let snap_after = state.snapshot();
        assert_eq!(snap_after.width, snap_before.width);
        let rows: Vec<Vec<char>> = snap_after
            .cells
            .chunks(snap_after.width)
            .map(|row| row.iter().map(|c| c.glyph).collect())
            .collect();
        for needle in batch
            .labels
            .iter()
            .map(|l| l.label.chars().collect::<Vec<char>>())
            .filter(|chars| chars.len() > 1)
        {
            for row in &rows {
                assert!(
                    row.windows(needle.len()).all(|w| w != needle.as_slice()),
                    "label '{}' must not appear in grid text",
                    needle.iter().collect::<String>()
                );
            }
        }
    }

    // -- chords, conflicts, session ------------------------------------------

    #[test]
    fn chord_parse_validates_everything() {
        let chord = parse_hint_chord('j', "ab").expect("valid chord");
        assert_eq!(chord.operator, HintOperator::Jump);
        assert_eq!(chord.label, "ab");
        assert_eq!(chord.operator.action(), HintAction::Jump);
        assert_eq!(HintOperator::ToggleFold.key(), 'z');
        assert_eq!(HintOperator::Focus.action(), HintAction::Focus);
        assert_eq!(HintOperator::Copy.action(), HintAction::Copy);
        assert_eq!(HintOperator::Expand.action(), HintAction::Expand);
        assert_eq!(HintOperator::Collapse.action(), HintAction::Collapse);
        assert_eq!(
            parse_hint_chord('q', "a"),
            Err(ChordError::UnknownOperator { key: 'q' })
        );
        assert_eq!(
            parse_hint_chord('J', "a"),
            Err(ChordError::UnknownOperator { key: 'J' })
        );
        assert_eq!(parse_hint_chord('j', ""), Err(ChordError::EmptyLabel));
        let long = "abcdefghi";
        assert_eq!(
            parse_hint_chord('j', long),
            Err(ChordError::LabelTooLong { chars: 9 })
        );
        assert_eq!(parse_hint_chord('j', "A"), Err(ChordError::LabelCharset));
        assert_eq!(parse_hint_chord('j', "a1"), Err(ChordError::LabelCharset));
    }

    #[test]
    fn conflicts_fail_closed_and_session_enforces() {
        // Default keymap binds no bare letters: arming succeeds.
        assert!(check_operator_conflicts(&[]).is_ok());
        assert!(check_operator_conflicts(&['1', '2', '\t']).is_ok());
        // Any bound bare operator letter blocks arming, case-insensitively.
        let err = check_operator_conflicts(&['x', 'J', 'z']).expect_err("conflict");
        assert_eq!(err.keys, vec!['j', 'z']);
        assert!(format!("{err}").contains("'j'"));

        // Unarmed feed never reaches the shell side nor the dispatcher.
        let session = HintSession::new();
        assert!(!session.is_armed());
        let mut fold = FoldState::new();
        assert_eq!(
            session.feed(&mut fold, 'j', "a"),
            Err(HintFeedError::NotArmed)
        );
        // Arming with a conflict keeps the session disarmed with no batch.
        let mut clash = HintSession::new();
        let batch = HintBatch {
            generation: 1,
            labels: Vec::new(),
            shed: 0,
        };
        assert!(clash.arm(batch, &['y']).is_err());
        assert!(!clash.is_armed());
        assert!(clash.batch().is_none());
        // Disarm drops the batch: hint chrome is ephemeral.
        let state = two_command_state();
        let (_, live_batch, _) = armed_session(&state);
        let mut live = HintSession::new();
        live.arm(live_batch, &[]).expect("arm");
        assert!(live.batch().is_some());
        live.disarm();
        assert!(!live.is_armed());
        assert!(live.batch().is_none());
        assert_eq!(live.feed(&mut fold, 'j', "a"), Err(HintFeedError::NotArmed));
    }

    #[test]
    fn hint_actions_set_algebra() {
        let mut set = HintActions::EMPTY;
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        set.insert(HintAction::Jump);
        set.insert(HintAction::Copy);
        assert!(!set.is_empty());
        assert_eq!(set.len(), 2);
        assert!(set.contains(HintAction::Jump));
        assert!(!set.contains(HintAction::Focus));
        let union = set | HintActions::FOCUS;
        assert_eq!(union.len(), 3);
        let ordered: Vec<HintAction> = HintActions::default_for(HintKind::View).iter().collect();
        assert_eq!(
            ordered,
            vec![HintAction::Focus, HintAction::Jump, HintAction::Copy]
        );
    }
}
