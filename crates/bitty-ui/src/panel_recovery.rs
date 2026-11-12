//! Unbound panel state, the placeholder reason vocabulary, and the capped
//! recovery budget (OQ-084 `panel-generation-ownership-decision.md`
//! Decision E; bitty#2476 residual 4).
//!
//! # Where the unbound panel lives
//!
//! The accepted `PanelState` keeps its **six** states
//! ([`crate::panel::PanelState`]). Decision C models an unbound panel as the
//! state it already held (`Mounted`, `Focused`, `Suspended`, or `Created`)
//! with **no controller bound** and **no content**: the container is retained,
//! the controller binding is absent, the content is gone. That is three facts
//! about one record, not a seventh lifecycle state, so this module models the
//! placeholder as a *reason plus the absence of a controller* rather than by
//! widening the state enum. Adding a seventh `PanelState` variant would both
//! break the accepted transition table and make "container retained" and
//! "content absent" indistinguishable at the state level.
//!
//! # What is decided here
//!
//! - [`PlaceholderReason`] — the seven placeholder reasons of Decision E rule
//!   2: the five shared with the GUI content release vocabulary
//!   (`suspended`, `reloaded`, `disabled`, `crashed`, `unloaded`) plus this
//!   record's `denied` and `incompatible`.
//! - [`RecoveryReason`] — the reason a transition *record* carries, which also
//!   spells the mapped close-side reason `closed` (`dismissed` and `quit` are
//!   accepted as aliases and normalise to it).
//! - [`RecoveryOutcome`] — Decision E's three legal outcomes (`Placeholder`,
//!   `Unavailable`, `Drop`) and the closed token vocabulary that selects one.
//!   A token outside the vocabulary is a typed refusal
//!   ([`RecoveryReasonError`]): never a placeholder reason, never passed
//!   through, and never a user-supplied string.
//! - [`RecoveryBudget`] — Decision E rule 1's counter, keyed
//!   `(PanelId, controller generation, reason)`, capped at
//!   [`RECOVERY_ATTEMPT_CAP`] (eight), with one `saturated` note per
//!   `(PanelId, binding)` and one FS-4-style [`RecoveryRecord`] per counted
//!   transition. The counter table is bounded per panel and makes room by
//!   evicting its oldest key, so a transition is never dropped for want of a
//!   slot; each record's `owner` is the panel record's own binding owner, never
//!   a caller-supplied string.
//! - [`PlaceholderScene`] — the Core-owned, fixed, non-plugin placeholder
//!   surface of rule 5: a constant text table under the accepted scene limits
//!   (`SCN-1..SCN-5`), with no caller-supplied text, no style input, and no
//!   constructor a plugin could reach.
//!
//! Everything here is headless, deterministic, bounded, and
//! `#![forbid(unsafe_code)]`. No wall clock, no randomness, no PTY, renderer,
//! or plugin handle participates: a placeholder is data plus a counter.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::panel::PanelId;

// ---------------------------------------------------------------------------
// Bounds (Decision E rule 1, SCN-1..SCN-5)
// ---------------------------------------------------------------------------

/// Recovery attempts counted per `(PanelId, controller generation, reason)`
/// (Decision E rule 1, "The counter is capped at eight").
///
/// At the cap the counter stops incrementing, the panel keeps its placeholder,
/// one `saturated` note is emitted per `(PanelId, binding)`, and further
/// attempts of the same reason change no state and emit nothing.
pub const RECOVERY_ATTEMPT_CAP: u8 = 8;

/// Distinct counter keys retained per panel.
///
/// One panel cannot accumulate an unbounded counter table: at most eight
/// `(generation, reason)` keys are tracked. A transition that would add a ninth
/// key **evicts the oldest tracked key** (the lowest-sorting pair — controller
/// generations only move forward, so that is the stalest binding) and is then
/// counted from one. The table therefore makes room instead of dropping the
/// transition: a panel that has used up its table still gets a counter, a
/// record, and — through the seam — its placeholder. Skipping the transition
/// instead would leave a released generation with no surface at all, the silent
/// orphan Decision E and PGO-23 forbid.
///
/// The cost of eviction is bounded and deliberate: an evicted key that comes
/// back is counted again from one, so the state retained per panel stays inside
/// `MAX_RECOVERY_KEYS_PER_PANEL` keys and
/// [`MAX_RECOVERY_RECORDS_PER_PANEL`] records no matter how many transitions a
/// container sees. The table is also cleared when the container is retired.
pub const MAX_RECOVERY_KEYS_PER_PANEL: usize = RECOVERY_ATTEMPT_CAP as usize;

/// FS-4-style records retained per panel for inspection.
///
/// The record for a transition is *returned* at the transition
/// ([`RecoveryStep::Recorded`]), so this ring only bounds the retained history:
/// it keeps the newest [`RECOVERY_ATTEMPT_CAP`] records and no diagnostic grows
/// with the number of attempts.
pub const MAX_RECOVERY_RECORDS_PER_PANEL: usize = RECOVERY_ATTEMPT_CAP as usize;

/// Longest reason spelling inspected before it is refused as too long.
///
/// An unknown token is echoed (bounded) in the typed refusal, so the bound
/// applies before the echo as well as before the lookup.
pub const MAX_REASON_SPELLING_LEN: usize = 32;

/// Longest owner spelling a [`RecoveryRecord`] can carry.
///
/// The owner is never a caller string: it is the panel record's own binding
/// owner, a `bitty_plugin_host::PluginId`, whose validation already bounds the
/// whole id (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*$`, at most
/// `MAX_PLUGIN_ID_LEN` = 128 bytes). `bitty-ui` does not depend on the plugin
/// host, so the bound is restated here and pinned against the enforcing value by
/// `crates/bitty-runtime/tests/panel_placeholder_bounds.rs`.
pub const MAX_OWNER_SPELLING_LEN: usize = 128;

/// `SCN-1`: max nodes per block (`rich-presentation-rfc.md`).
///
/// Duplicated from the accepted table because `bitty-ui` does not depend on
/// the scene crate; the enforcement point is `bitty_rich::scene::RichBlock::new`
/// and `crates/bitty-runtime/tests/panel_placeholder_bounds.rs` asserts these
/// stay equal to the enforced values.
pub const SCN_MAX_NODES_PER_BLOCK: usize = 2048;

/// `SCN-2`: max tree depth per block.
pub const SCN_MAX_DEPTH_PER_BLOCK: usize = 32;

/// `SCN-3`: max text bytes per block.
pub const SCN_MAX_TEXT_BYTES_PER_BLOCK: usize = 256 * 1024;

/// `SCN-4`: max aggregated rich bytes per terminal.
pub const SCN_MAX_RICH_BYTES_PER_TERMINAL: usize = 2 * 1024 * 1024;

/// `SCN-5`: max blocks per terminal.
pub const SCN_MAX_BLOCKS_PER_TERMINAL: usize = 64;

/// Nodes in the fixed placeholder scene: one column, two text lines, and a row
/// of two action labels (counted the way `SceneNode::count_nodes` counts them).
pub const PLACEHOLDER_SCENE_NODES: usize = 6;

/// Tree depth of the fixed placeholder scene.
pub const PLACEHOLDER_SCENE_DEPTH: usize = 3;

/// Actions offered by the placeholder surface: close and retry (Decision E,
/// "offers close and retry actions").
pub const PLACEHOLDER_SCENE_ACTIONS: usize = 2;

/// Heading of the fixed placeholder surface.
///
/// One Core-owned heading for every reason: the reason is named on the detail
/// line, and "unavailable" is the only surface word this record decides.
pub const PLACEHOLDER_HEADING: &str = "Panel unavailable";

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A placeholder reason that the closed vocabulary does not contain.
///
/// Decision E rule 2: "A reason outside the resulting set is a defect, and a
/// user-supplied reason is never accepted." Every variant is a refusal: no
/// caller can turn one of these into a placeholder, and the spelling is echoed
/// only after [`MAX_REASON_SPELLING_LEN`] has bounded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryReasonError {
    /// The submitted token was empty.
    Empty,
    /// The submitted token exceeded [`MAX_REASON_SPELLING_LEN`] bytes.
    TooLong {
        /// Bytes counted in the submitted token.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The submitted token is not in the canonical lowercase spelling shape.
    Charset {
        /// The refused token, already bounded.
        spelling: String,
    },
    /// The token is well formed but names no reason in the closed vocabulary.
    Unknown {
        /// The refused token, already bounded.
        spelling: String,
    },
}

impl RecoveryReasonError {
    /// The refused spelling, bounded by [`MAX_REASON_SPELLING_LEN`].
    ///
    /// Empty for [`Self::Empty`] and [`Self::TooLong`], because neither
    /// variant retains the submitted text.
    #[must_use]
    pub fn spelling(&self) -> &str {
        match self {
            Self::Empty | Self::TooLong { .. } => "",
            Self::Charset { spelling } | Self::Unknown { spelling } => spelling,
        }
    }
}

impl fmt::Display for RecoveryReasonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("placeholder reason is empty"),
            Self::TooLong { found, cap } => {
                write!(
                    f,
                    "placeholder reason is too long: {found} bytes, cap {cap}"
                )
            }
            Self::Charset { spelling } => {
                write!(
                    f,
                    "placeholder reason {spelling:?} is not a lowercase token"
                )
            }
            Self::Unknown { spelling } => {
                write!(
                    f,
                    "placeholder reason {spelling:?} is outside the vocabulary"
                )
            }
        }
    }
}

impl std::error::Error for RecoveryReasonError {}

// ---------------------------------------------------------------------------
// Placeholder reasons (Decision E rule 2: seven)
// ---------------------------------------------------------------------------

/// The reason a placeholder names (Decision E rule 2).
///
/// Closed set: the five reasons shared with the GUI content release vocabulary
/// plus the two this record adds. The close-side tokens (`closed`,
/// `dismissed`, `quit`) are *not* placeholder reasons — they select the `Drop`
/// outcome — so they have no variant here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlaceholderReason {
    /// The controller generation is `Suspended`; the panel keeps its container
    /// and state and loses its controller verbs (C.1).
    Suspended,
    /// A successor generation is mid-reload (C.5, C.6).
    Reloaded,
    /// The plugin is disabled by the user or by policy; re-enabling permits a
    /// rebind (C.4).
    Disabled,
    /// The generation faulted and ended `Disabled` through the FS-2 ladder
    /// (C.3).
    Crashed,
    /// The provider is not installed. Placeholders stay until the user closes
    /// them (E.4).
    Unloaded,
    /// A required capability is denied, or safe mode excludes the plugin
    /// (E.2, `Unavailable` outcome).
    Denied,
    /// Persisted content was produced under a newer schema than the provider
    /// supports; nothing is downgraded or partially applied (E.3).
    Incompatible,
}

impl PlaceholderReason {
    /// Every placeholder reason, in vocabulary order.
    pub const ALL: [Self; 7] = [
        Self::Suspended,
        Self::Reloaded,
        Self::Disabled,
        Self::Crashed,
        Self::Unloaded,
        Self::Denied,
        Self::Incompatible,
    ];

    /// Canonical lowercase spelling, shared with the content vocabulary.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Suspended => "suspended",
            Self::Reloaded => "reloaded",
            Self::Disabled => "disabled",
            Self::Crashed => "crashed",
            Self::Unloaded => "unloaded",
            Self::Denied => "denied",
            Self::Incompatible => "incompatible",
        }
    }

    /// The Core-owned placeholder detail line, naming the reason.
    ///
    /// These strings are the whole user-visible payload of a placeholder: no
    /// plugin string reaches this surface (rule 5).
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Suspended => "Its plugin controller is suspended.",
            Self::Reloaded => "Its plugin is reloading.",
            Self::Disabled => "Its plugin is disabled. Re-enable it to rebind.",
            Self::Crashed => "Its plugin controller stopped after a fault.",
            Self::Unloaded => "Its plugin is not installed.",
            Self::Denied => "Its plugin is not permitted to present a panel.",
            Self::Incompatible => "Its content needs a newer plugin version.",
        }
    }
}

impl fmt::Display for PlaceholderReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Recovery reasons: placeholder reasons plus the mapped close reason
// ---------------------------------------------------------------------------

/// The reason carried by a recovery transition record (Decision E rules 1-2).
///
/// A superset of [`PlaceholderReason`] with exactly one addition,
/// [`Self::Closed`], which is where the content vocabulary's `closed`,
/// `dismissed`, and `quit` map: they are user-initiated ends of the content, so
/// they select the `Drop` outcome instead of a placeholder reason. The three
/// spellings normalise to one, so no diagnostic is split across two spellings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecoveryReason {
    /// See [`PlaceholderReason::Suspended`].
    Suspended,
    /// See [`PlaceholderReason::Reloaded`].
    Reloaded,
    /// See [`PlaceholderReason::Disabled`].
    Disabled,
    /// See [`PlaceholderReason::Crashed`].
    Crashed,
    /// See [`PlaceholderReason::Unloaded`].
    Unloaded,
    /// See [`PlaceholderReason::Denied`].
    Denied,
    /// See [`PlaceholderReason::Incompatible`].
    Incompatible,
    /// The user closed the panel, closed the workspace, or discarded the
    /// plugin's panels; the container is disposed (`Drop`).
    Closed,
}

impl RecoveryReason {
    /// Every recovery reason, in vocabulary order.
    pub const ALL: [Self; 8] = [
        Self::Suspended,
        Self::Reloaded,
        Self::Disabled,
        Self::Crashed,
        Self::Unloaded,
        Self::Denied,
        Self::Incompatible,
        Self::Closed,
    ];

    /// Canonical lowercase spelling. `dismissed` and `quit` normalise here.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Suspended => "suspended",
            Self::Reloaded => "reloaded",
            Self::Disabled => "disabled",
            Self::Crashed => "crashed",
            Self::Unloaded => "unloaded",
            Self::Denied => "denied",
            Self::Incompatible => "incompatible",
            Self::Closed => "closed",
        }
    }

    /// Parses one token of the closed vocabulary.
    ///
    /// This is the only door into a reason, and it is closed: a token outside
    /// the set is a typed refusal, never a reason. `dismissed` and `quit` are
    /// accepted as aliases of `closed` (E.2).
    ///
    /// # Errors
    /// [`RecoveryReasonError`] for an empty, oversized, non-lowercase, or
    /// unknown token.
    pub fn parse(raw: &str) -> Result<Self, RecoveryReasonError> {
        if raw.is_empty() {
            return Err(RecoveryReasonError::Empty);
        }
        if raw.len() > MAX_REASON_SPELLING_LEN {
            return Err(RecoveryReasonError::TooLong {
                found: raw.len(),
                cap: MAX_REASON_SPELLING_LEN,
            });
        }
        if !raw.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
            return Err(RecoveryReasonError::Charset {
                spelling: raw.to_owned(),
            });
        }
        match raw {
            "suspended" => Ok(Self::Suspended),
            "reloaded" => Ok(Self::Reloaded),
            "disabled" => Ok(Self::Disabled),
            "crashed" => Ok(Self::Crashed),
            "unloaded" => Ok(Self::Unloaded),
            "denied" => Ok(Self::Denied),
            "incompatible" => Ok(Self::Incompatible),
            "closed" | "dismissed" | "quit" => Ok(Self::Closed),
            _ => Err(RecoveryReasonError::Unknown {
                spelling: raw.to_owned(),
            }),
        }
    }

    /// The placeholder reason this reason names, or `None` for
    /// [`Self::Closed`].
    #[must_use]
    pub const fn placeholder(self) -> Option<PlaceholderReason> {
        match self {
            Self::Suspended => Some(PlaceholderReason::Suspended),
            Self::Reloaded => Some(PlaceholderReason::Reloaded),
            Self::Disabled => Some(PlaceholderReason::Disabled),
            Self::Crashed => Some(PlaceholderReason::Crashed),
            Self::Unloaded => Some(PlaceholderReason::Unloaded),
            Self::Denied => Some(PlaceholderReason::Denied),
            Self::Incompatible => Some(PlaceholderReason::Incompatible),
            Self::Closed => None,
        }
    }
}

impl From<PlaceholderReason> for RecoveryReason {
    fn from(reason: PlaceholderReason) -> Self {
        match reason {
            PlaceholderReason::Suspended => Self::Suspended,
            PlaceholderReason::Reloaded => Self::Reloaded,
            PlaceholderReason::Disabled => Self::Disabled,
            PlaceholderReason::Crashed => Self::Crashed,
            PlaceholderReason::Unloaded => Self::Unloaded,
            PlaceholderReason::Denied => Self::Denied,
            PlaceholderReason::Incompatible => Self::Incompatible,
        }
    }
}

impl fmt::Display for RecoveryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// The three legal outcomes (Decision E table)
// ---------------------------------------------------------------------------

/// What a recovery transition did with the panel (Decision E's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryOutcome {
    /// The container stays with its title and geometry, presents the Core
    /// placeholder naming `reason`, accepts no content verb, and offers close
    /// and retry.
    Placeholder(PlaceholderReason),
    /// The container stays and presents "unavailable" with the denial named
    /// (`denied`: a required capability is denied, or safe mode excludes the
    /// plugin). No content verb is reachable.
    Unavailable,
    /// The user closed the panel or the workspace, or accepted "discard panels
    /// of this plugin": the container is disposed and the loss reported.
    Drop,
}

impl RecoveryOutcome {
    /// Classifies one vocabulary token into Decision E's three outcomes.
    ///
    /// # Errors
    /// [`RecoveryReasonError`] when the token is outside the vocabulary. A
    /// refused token produces no outcome at all, so no placeholder can be
    /// presented for it.
    pub fn classify(token: &str) -> Result<Self, RecoveryReasonError> {
        Ok(Self::from_reason(RecoveryReason::parse(token)?))
    }

    /// Classifies an already-parsed reason.
    #[must_use]
    pub const fn from_reason(reason: RecoveryReason) -> Self {
        match reason {
            RecoveryReason::Suspended => Self::Placeholder(PlaceholderReason::Suspended),
            RecoveryReason::Reloaded => Self::Placeholder(PlaceholderReason::Reloaded),
            RecoveryReason::Disabled => Self::Placeholder(PlaceholderReason::Disabled),
            RecoveryReason::Crashed => Self::Placeholder(PlaceholderReason::Crashed),
            RecoveryReason::Unloaded => Self::Placeholder(PlaceholderReason::Unloaded),
            RecoveryReason::Incompatible => Self::Placeholder(PlaceholderReason::Incompatible),
            RecoveryReason::Denied => Self::Unavailable,
            RecoveryReason::Closed => Self::Drop,
        }
    }

    /// The reason this outcome records (`denied` for [`Self::Unavailable`],
    /// `closed` for [`Self::Drop`]).
    #[must_use]
    pub const fn reason(self) -> RecoveryReason {
        match self {
            Self::Placeholder(PlaceholderReason::Suspended) => RecoveryReason::Suspended,
            Self::Placeholder(PlaceholderReason::Reloaded) => RecoveryReason::Reloaded,
            Self::Placeholder(PlaceholderReason::Disabled) => RecoveryReason::Disabled,
            Self::Placeholder(PlaceholderReason::Crashed) => RecoveryReason::Crashed,
            Self::Placeholder(PlaceholderReason::Unloaded) => RecoveryReason::Unloaded,
            Self::Placeholder(PlaceholderReason::Denied) => RecoveryReason::Denied,
            Self::Placeholder(PlaceholderReason::Incompatible) => RecoveryReason::Incompatible,
            Self::Unavailable => RecoveryReason::Denied,
            Self::Drop => RecoveryReason::Closed,
        }
    }

    /// The action this outcome performs, for the transition record.
    #[must_use]
    pub const fn action(self) -> RecoveryAction {
        match self {
            Self::Placeholder(_) => RecoveryAction::Placeholder,
            Self::Unavailable => RecoveryAction::Unavailable,
            Self::Drop => RecoveryAction::Disposed,
        }
    }

    /// Whether this outcome retires the container.
    #[must_use]
    pub const fn disposes_container(self) -> bool {
        matches!(self, Self::Drop)
    }

    /// The placeholder reason this outcome presents, or `None` for
    /// [`Self::Drop`], which presents no surface.
    #[must_use]
    pub const fn placeholder_reason(self) -> Option<PlaceholderReason> {
        match self {
            Self::Placeholder(reason) => Some(reason),
            Self::Unavailable => Some(PlaceholderReason::Denied),
            Self::Drop => None,
        }
    }
}

impl fmt::Display for RecoveryOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.action().as_str())
    }
}

/// The action a recovery transition took, carried by its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecoveryAction {
    /// The container was kept and the placeholder presented.
    Placeholder,
    /// The container was kept and the unavailable surface presented.
    Unavailable,
    /// The container was disposed through `PanelRegistry::dispose_panel`.
    Disposed,
}

impl RecoveryAction {
    /// Canonical lowercase spelling for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Placeholder => "placeholder",
            Self::Unavailable => "unavailable",
            Self::Disposed => "disposed",
        }
    }
}

impl fmt::Display for RecoveryAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// FS-4-style transition record
// ---------------------------------------------------------------------------

/// One FS-4-style recovery record: owner, generation, reason, action.
///
/// Decision E rule 1 requires one such record per counted transition. The owner
/// is the panel record's **own** binding owner ([`RecoveryRecord::owner`]), read
/// by the seam that presents the placeholder before the unbind edge clears the
/// binding — never a string the caller asserts. A transition can therefore not
/// be attributed to a plugin the panel never bound, and the field is `None`
/// exactly while no controller is bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryRecord {
    /// The unbound panel.
    pub panel: PanelId,
    /// The plugin that held the content when the transition happened, taken
    /// from the panel record's binding.
    ///
    /// `None` when no controller is bound: a container that never had one
    /// (`unloaded`, and the `Drop` records of a panel disposed before its first
    /// bind). `Some` is the binding's `PluginId` spelling, which validation
    /// bounds to [`MAX_OWNER_SPELLING_LEN`] bytes.
    pub owner: Option<String>,
    /// The controller generation the transition names.
    pub controller_generation: u64,
    /// Why the panel became unbound (or why it was disposed).
    pub reason: RecoveryReason,
    /// What the transition did.
    pub action: RecoveryAction,
    /// The counted attempt number, `1..=RECOVERY_ATTEMPT_CAP`; `0` for a
    /// `Drop` record, which is not a recovery attempt.
    pub attempt: u8,
}

// ---------------------------------------------------------------------------
// The capped counter
// ---------------------------------------------------------------------------

/// What one [`RecoveryBudget::note`] call did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryStep {
    /// Counted and emitted: this is the transition's record.
    Recorded(RecoveryRecord),
    /// The counter is at its cap: the counter did not move, the panel keeps its
    /// placeholder, and this call emitted the single `saturated` note for this
    /// binding.
    Saturated {
        /// The unbound panel.
        panel: PanelId,
        /// The binding the note is scoped to.
        controller_generation: u64,
        /// The reason the superseded attempt carried.
        reason: RecoveryReason,
        /// The counter's value at the cap.
        attempt: u8,
    },
    /// The counter is at its cap and the `saturated` note for this binding has
    /// already gone out: nothing changed and nothing was emitted.
    Unchanged,
}

/// The bounded recovery counter of Decision E rule 1.
///
/// Keys are `(PanelId, controller generation, reason)`; each counter stops at
/// [`RECOVERY_ATTEMPT_CAP`]. The table is bounded per panel
/// ([`MAX_RECOVERY_KEYS_PER_PANEL`] keys and [`MAX_RECOVERY_RECORDS_PER_PANEL`]
/// retained records, the oldest key evicted to make room for a new one) and is
/// cleared when the container is retired ([`Self::retire`]), so no diagnostic
/// grows with the number of attempts.
#[derive(Debug, Default)]
pub struct RecoveryBudget {
    counts: BTreeMap<PanelId, BTreeMap<(u64, RecoveryReason), u8>>,
    saturated: BTreeMap<PanelId, u64>,
    records: BTreeMap<PanelId, VecDeque<RecoveryRecord>>,
}

impl RecoveryBudget {
    /// An empty budget.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one transition and returns what it did.
    ///
    /// The record's action is derived from `reason` through
    /// [`RecoveryOutcome::from_reason`], the single Core mapping, so a record
    /// can never pair a reason with an action the reason does not select
    /// (`denied` is always `Unavailable`, `closed` is always `Disposed`).
    ///
    /// `owner` is the panel record's own binding owner — the seam reads it from
    /// the record rather than taking a caller's word for it — and is `None`
    /// exactly while no controller is bound.
    ///
    /// A [`RecoveryReason::Closed`] transition is not counted: the container is
    /// gone, so there is nothing to bound, and its record carries
    /// `attempt == 0`. Every other transition increments exactly one counter
    /// keyed `(panel, controller_generation, reason)` and yields
    /// [`RecoveryStep::Recorded`] while the counter is below the cap. A key
    /// arriving at a full table is recorded too: the table evicts its oldest
    /// key to make room (see [`MAX_RECOVERY_KEYS_PER_PANEL`]) instead of
    /// answering the transition with nothing, so the caller always has a record
    /// to present against.
    pub fn note(
        &mut self,
        panel: PanelId,
        owner: Option<&str>,
        controller_generation: u64,
        reason: RecoveryReason,
    ) -> RecoveryStep {
        let outcome = RecoveryOutcome::from_reason(reason);
        let action = outcome.action();
        let owner = owner.map(str::to_owned);
        if outcome.disposes_container() {
            let record = RecoveryRecord {
                panel,
                owner,
                controller_generation,
                reason,
                action,
                attempt: 0,
            };
            return RecoveryStep::Recorded(record);
        }
        let key = (controller_generation, reason);
        let counted;
        let mut at_cap = false;
        {
            let slots = self.counts.entry(panel).or_default();
            if !slots.contains_key(&key) && slots.len() >= MAX_RECOVERY_KEYS_PER_PANEL {
                // Make room: the counter table is bounded, the transition is
                // not. Evicting the lowest-sorting pair drops the stalest
                // binding (generations only move forward) and lets this key
                // count from one, so a full table never swallows a transition
                // that has already released its generation.
                let oldest = slots.keys().next().copied();
                if let Some(oldest) = oldest {
                    slots.remove(&oldest);
                }
            }
            let counter = slots.entry(key).or_insert(0);
            if *counter >= RECOVERY_ATTEMPT_CAP {
                at_cap = true;
                counted = *counter;
            } else {
                *counter += 1;
                counted = *counter;
            }
        }
        if at_cap {
            if self.saturated.get(&panel) == Some(&controller_generation) {
                return RecoveryStep::Unchanged;
            }
            self.saturated.insert(panel, controller_generation);
            return RecoveryStep::Saturated {
                panel,
                controller_generation,
                reason,
                attempt: counted,
            };
        }
        let record = RecoveryRecord {
            panel,
            owner,
            controller_generation,
            reason,
            action,
            attempt: counted,
        };
        let history = self.records.entry(panel).or_default();
        if history.len() >= MAX_RECOVERY_RECORDS_PER_PANEL {
            history.pop_front();
        }
        history.push_back(record.clone());
        RecoveryStep::Recorded(record)
    }

    /// The counted attempts for one `(panel, generation, reason)` key.
    #[must_use]
    pub fn count(&self, panel: PanelId, controller_generation: u64, reason: RecoveryReason) -> u8 {
        self.counts
            .get(&panel)
            .and_then(|slots| slots.get(&(controller_generation, reason)))
            .copied()
            .unwrap_or(0)
    }

    /// The binding the single `saturated` note already went out for, if any.
    #[must_use]
    pub fn saturated_note(&self, panel: PanelId) -> Option<u64> {
        self.saturated.get(&panel).copied()
    }

    /// The retained records for one panel, oldest first.
    #[must_use]
    pub fn records(&self, panel: PanelId) -> Vec<RecoveryRecord> {
        self.records
            .get(&panel)
            .map(|history| history.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The most recent retained record for one panel.
    #[must_use]
    pub fn last_record(&self, panel: PanelId) -> Option<RecoveryRecord> {
        self.records
            .get(&panel)
            .and_then(|history| history.back().cloned())
    }

    /// Drops every counter, saturation note, and record for a retired
    /// container.
    pub fn retire(&mut self, panel: PanelId) {
        self.counts.remove(&panel);
        self.saturated.remove(&panel);
        self.records.remove(&panel);
    }

    /// Panels with any retained recovery state.
    #[must_use]
    pub fn tracked_panels(&self) -> usize {
        let mut panels: BTreeSet<PanelId> = self.counts.keys().copied().collect();
        panels.extend(self.saturated.keys().copied());
        panels.extend(self.records.keys().copied());
        panels.len()
    }

    /// Retained records across every panel.
    #[must_use]
    pub fn total_records(&self) -> usize {
        self.records.values().map(VecDeque::len).sum()
    }

    /// Distinct `(controller generation, reason)` counter keys tracked for one
    /// panel, always `<= MAX_RECOVERY_KEYS_PER_PANEL`.
    #[must_use]
    pub fn tracked_keys(&self, panel: PanelId) -> usize {
        self.counts.get(&panel).map(BTreeMap::len).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Core-owned placeholder surface (Decision E rule 5)
// ---------------------------------------------------------------------------

/// One action offered by the Core placeholder surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlaceholderAction {
    /// Route to the disposal path (`Drop`).
    Close,
    /// Ask the caller to re-attempt the bind. The placeholder itself performs
    /// nothing: rule 6 keeps the recovery surface to presentation plus
    /// commands.
    Retry,
}

impl PlaceholderAction {
    /// The actions the fixed surface offers, in display order.
    pub const ALL: [Self; PLACEHOLDER_SCENE_ACTIONS] = [Self::Close, Self::Retry];

    /// Canonical lowercase spelling (the command token a caller routes on).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Retry => "retry",
        }
    }

    /// The Core-owned label painted on the surface.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Close => "Close",
            Self::Retry => "Retry",
        }
    }
}

impl fmt::Display for PlaceholderAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The Core-owned, fixed placeholder surface (Decision E rule 5).
///
/// Its whole visible payload is [`PLACEHOLDER_HEADING`], the constant table in
/// [`PlaceholderReason::detail`], and the [`PlaceholderAction`] labels. There is
/// deliberately no constructor that accepts text, a style, a node tree, or a
/// replacement scene: a plugin cannot style this surface, cannot place text on
/// it, and cannot substitute one of its own. The shape is fixed at
/// [`PLACEHOLDER_SCENE_NODES`] nodes and
/// [`PLACEHOLDER_SCENE_DEPTH`] levels, far inside `SCN-1..SCN-5`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaceholderScene {
    heading: &'static str,
    detail: &'static str,
    actions: [PlaceholderAction; PLACEHOLDER_SCENE_ACTIONS],
}

impl PlaceholderScene {
    /// The fixed surface for one placeholder reason.
    #[must_use]
    pub const fn for_reason(reason: PlaceholderReason) -> Self {
        Self {
            heading: PLACEHOLDER_HEADING,
            detail: reason.detail(),
            actions: PlaceholderAction::ALL,
        }
    }

    /// The fixed surface for the `Unavailable` outcome (`denied`).
    #[must_use]
    pub const fn unavailable() -> Self {
        Self::for_reason(PlaceholderReason::Denied)
    }

    /// The heading line (Core text).
    #[must_use]
    pub const fn heading(&self) -> &'static str {
        self.heading
    }

    /// The detail line naming the reason (Core text).
    #[must_use]
    pub const fn detail(&self) -> &'static str {
        self.detail
    }

    /// The offered actions, in display order.
    #[must_use]
    pub const fn actions(&self) -> [PlaceholderAction; PLACEHOLDER_SCENE_ACTIONS] {
        self.actions
    }

    /// Nodes the surface occupies (`SCN-1` counts them this way).
    #[must_use]
    pub const fn node_count(&self) -> usize {
        PLACEHOLDER_SCENE_NODES
    }

    /// Tree depth of the surface (`SCN-2`).
    #[must_use]
    pub const fn depth(&self) -> usize {
        PLACEHOLDER_SCENE_DEPTH
    }

    /// Text bytes the surface carries (`SCN-3`), summed exactly as
    /// `SceneNode::text_bytes` sums it.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        self.heading.len()
            + self.detail.len()
            + self
                .actions
                .iter()
                .map(|action| action.label().len())
                .sum::<usize>()
    }

    /// Whether the surface is inside every accepted scene limit it touches.
    #[must_use]
    pub fn within_scn_limits(&self) -> bool {
        self.node_count() <= SCN_MAX_NODES_PER_BLOCK
            && self.depth() <= SCN_MAX_DEPTH_PER_BLOCK
            && self.text_bytes() <= SCN_MAX_TEXT_BYTES_PER_BLOCK
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(raw: u64) -> PanelId {
        PanelId::new(raw)
    }

    /// The owner a fake seam supplies. In the registry this value is never
    /// chosen by the caller: `present_placeholder` reads it from the panel
    /// record's own binding (`PanelRecord::owner`), and it is `None` while no
    /// controller is bound.
    const OWNER: &str = "acme.notes";

    /// E.2: the vocabulary is exactly seven placeholder reasons plus the mapped
    /// close reason, and every base spelling round-trips. A wrong
    /// implementation that dropped `denied` or `incompatible`, or that kept
    /// `dismissed` as a separate reason, fails here.
    #[test]
    fn the_reason_vocabulary_is_the_superset_with_one_mapping() {
        assert_eq!(PlaceholderReason::ALL.len(), 7);
        assert_eq!(RecoveryReason::ALL.len(), 8);

        let placeholder_spellings: Vec<&str> =
            PlaceholderReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            placeholder_spellings,
            vec![
                "suspended",
                "reloaded",
                "disabled",
                "crashed",
                "unloaded",
                "denied",
                "incompatible"
            ]
        );

        for reason in RecoveryReason::ALL {
            assert_eq!(
                RecoveryReason::parse(reason.as_str()),
                Ok(reason),
                "{reason} must parse from its own spelling"
            );
        }
        assert_eq!(
            RecoveryReason::parse("dismissed"),
            Ok(RecoveryReason::Closed)
        );
        assert_eq!(RecoveryReason::parse("quit"), Ok(RecoveryReason::Closed));
        assert_eq!(RecoveryReason::parse("closed"), Ok(RecoveryReason::Closed));
        // One spelling for the three close-side tokens: no diagnostic splits.
        assert_eq!(RecoveryReason::Closed.as_str(), "closed");
    }

    /// E.2: a token outside the set is a typed refusal, never a reason, and the
    /// echo is bounded. A wrong implementation that returns a default reason or
    /// stores arbitrary text fails here.
    #[test]
    fn a_token_outside_the_vocabulary_is_refused_and_bounded() {
        assert_eq!(RecoveryReason::parse(""), Err(RecoveryReasonError::Empty));
        for hostile in [
            "Closed",
            "CRASHED",
            "crashed!",
            " crashed",
            "quitted",
            "Panel crashed",
        ] {
            assert!(
                RecoveryReason::parse(hostile).is_err(),
                "{hostile:?} must not parse"
            );
        }
        assert_eq!(
            RecoveryReason::parse("quitted"),
            Err(RecoveryReasonError::Unknown {
                spelling: "quitted".to_owned()
            })
        );
        let long = "a".repeat(MAX_REASON_SPELLING_LEN + 1);
        match RecoveryReason::parse(&long) {
            Err(RecoveryReasonError::TooLong { found, cap }) => {
                assert_eq!(cap, MAX_REASON_SPELLING_LEN);
                assert_eq!(found, MAX_REASON_SPELLING_LEN + 1);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
        // The refusal never carries the oversized payload onward.
        assert_eq!(
            RecoveryReason::parse(&long)
                .err()
                .map(|error| error.spelling().to_owned()),
            Some(String::new())
        );
    }

    /// E's table: three outcomes, `denied` selects `Unavailable`, the three
    /// close tokens select `Drop`, everything else presents a placeholder. A
    /// wrong implementation that presents a placeholder for `closed` (the
    /// orphan this record forbids) fails here.
    #[test]
    fn the_three_outcomes_are_selected_from_the_vocabulary() {
        assert_eq!(
            RecoveryOutcome::classify("crashed"),
            Ok(RecoveryOutcome::Placeholder(PlaceholderReason::Crashed))
        );
        assert_eq!(
            RecoveryOutcome::classify("denied"),
            Ok(RecoveryOutcome::Unavailable)
        );
        assert_eq!(
            RecoveryOutcome::classify("incompatible"),
            Ok(RecoveryOutcome::Placeholder(
                PlaceholderReason::Incompatible
            ))
        );
        for token in ["closed", "dismissed", "quit"] {
            assert_eq!(
                RecoveryOutcome::classify(token),
                Ok(RecoveryOutcome::Drop),
                "{token} must select Drop"
            );
        }
        assert!(
            RecoveryOutcome::classify("closed")
                .unwrap()
                .disposes_container()
        );
        assert!(
            !RecoveryOutcome::classify("crashed")
                .unwrap()
                .disposes_container()
        );

        // The record vocabulary for the two non-placeholder outcomes.
        assert_eq!(
            RecoveryOutcome::Unavailable.reason(),
            RecoveryReason::Denied
        );
        assert_eq!(RecoveryOutcome::Drop.reason(), RecoveryReason::Closed);
        assert_eq!(
            RecoveryOutcome::Unavailable.action(),
            RecoveryAction::Unavailable
        );
        assert_eq!(RecoveryOutcome::Drop.action(), RecoveryAction::Disposed);
    }

    /// E.1: eight attempts are counted and recorded; the ninth changes nothing,
    /// emits one saturated note, and later attempts emit nothing at all. A
    /// wrong implementation with an unbounded counter or a per-attempt
    /// diagnostic grows past eight and fails here.
    #[test]
    fn the_counter_stops_at_eight_and_saturates_once_per_binding() {
        let mut budget = RecoveryBudget::new();
        let id = panel(7);
        let binding = 42;
        let reason = RecoveryReason::Crashed;
        for attempt in 1..=RECOVERY_ATTEMPT_CAP {
            match budget.note(id, Some(OWNER), binding, reason) {
                RecoveryStep::Recorded(record) => {
                    assert_eq!(record.attempt, attempt);
                    assert_eq!(record.reason, reason);
                    assert_eq!(record.controller_generation, binding);
                    assert_eq!(record.panel, id);
                }
                other => panic!("attempt {attempt} must be counted, got {other:?}"),
            }
            assert_eq!(budget.count(id, binding, reason), attempt);
        }
        assert_eq!(budget.count(id, binding, reason), RECOVERY_ATTEMPT_CAP);

        let saturated = budget.note(id, Some(OWNER), binding, reason);
        match saturated {
            RecoveryStep::Saturated {
                attempt,
                controller_generation,
                reason: noted,
                ..
            } => {
                assert_eq!(attempt, RECOVERY_ATTEMPT_CAP);
                assert_eq!(controller_generation, binding);
                assert_eq!(noted, reason);
            }
            other => panic!("expected the saturated note, got {other:?}"),
        }
        assert_eq!(budget.count(id, binding, reason), RECOVERY_ATTEMPT_CAP);
        assert_eq!(budget.saturated_note(id), Some(binding));

        // Further attempts of the same reason: no state change, no emission.
        for _ in 0..4 {
            assert_eq!(
                budget.note(id, Some(OWNER), binding, reason),
                RecoveryStep::Unchanged
            );
        }
        assert_eq!(budget.count(id, binding, reason), RECOVERY_ATTEMPT_CAP);
        assert_eq!(budget.records(id).len(), MAX_RECOVERY_RECORDS_PER_PANEL);
    }

    /// E.1: the key includes the reason and the controller generation, so a new
    /// binding or a different reason gets its own fresh counter. A wrong
    /// implementation keyed by the panel alone (or by the panel and generation)
    /// reports a saturated panel on a rebind and fails here.
    #[test]
    fn the_counter_key_carries_the_reason_and_the_generation() {
        let mut budget = RecoveryBudget::new();
        let id = panel(3);
        for _ in 0..RECOVERY_ATTEMPT_CAP {
            budget.note(id, Some(OWNER), 10, RecoveryReason::Crashed);
        }
        assert_eq!(
            budget.note(id, Some(OWNER), 10, RecoveryReason::Crashed),
            RecoveryStep::Saturated {
                panel: id,
                controller_generation: 10,
                reason: RecoveryReason::Crashed,
                attempt: RECOVERY_ATTEMPT_CAP,
            }
        );
        // A different reason is a different key and is still counted.
        match budget.note(id, Some(OWNER), 10, RecoveryReason::Suspended) {
            RecoveryStep::Recorded(record) => {
                assert_eq!(record.attempt, 1);
                assert_eq!(
                    record.owner.as_deref(),
                    Some(OWNER),
                    "the record carries the owner it was given, and nothing else stamps it"
                );
            }
            other => panic!("a second reason must count from one, got {other:?}"),
        }
        // A new binding re-arms the saturated note and counts from one again.
        assert_eq!(
            budget.note(id, Some(OWNER), 10, RecoveryReason::Crashed),
            RecoveryStep::Unchanged
        );
        match budget.note(id, Some(OWNER), 11, RecoveryReason::Crashed) {
            RecoveryStep::Recorded(record) => {
                assert_eq!(record.controller_generation, 11);
                assert_eq!(record.attempt, 1);
            }
            other => panic!("a new binding must count from one, got {other:?}"),
        }
        assert_eq!(budget.saturated_note(id), Some(10));
    }

    /// E.1 carried into the retained state: the key table and the record ring
    /// are bounded per panel, and a key that arrives at a full table **makes
    /// room** instead of being answered with nothing — the transition is still
    /// counted and still recorded, so a panel that has already released its
    /// generation is never left without a record to present. Fails for an
    /// implementation that appends a record per attempt, or that drops the
    /// transition when the table is full.
    #[test]
    fn the_retained_table_and_history_are_bounded_per_panel() {
        let mut budget = RecoveryBudget::new();
        let id = panel(5);
        // The seven counted reasons fill seven of the eight keys at generation
        // 1. `closed` is a disposal: recorded, never counted, never keyed.
        for reason in RecoveryReason::ALL {
            if reason.placeholder().is_none() {
                continue;
            }
            match budget.note(id, Some(OWNER), 1, reason) {
                RecoveryStep::Recorded(record) => {
                    assert_eq!(record.attempt, 1);
                    assert_eq!(record.owner.as_deref(), Some(OWNER));
                }
                other => panic!("{reason} must count, got {other:?}"),
            }
        }
        assert_eq!(budget.tracked_keys(id), MAX_RECOVERY_KEYS_PER_PANEL - 1);
        // The eighth key fills the table.
        assert!(matches!(
            budget.note(id, Some(OWNER), 2, RecoveryReason::Suspended),
            RecoveryStep::Recorded(_)
        ));
        assert_eq!(budget.tracked_keys(id), MAX_RECOVERY_KEYS_PER_PANEL);
        // A ninth key makes room instead of being dropped: it counts from one
        // and the stalest key (the lowest generation) is evicted.
        match budget.note(id, Some(OWNER), 3, RecoveryReason::Crashed) {
            RecoveryStep::Recorded(record) => assert_eq!(record.attempt, 1),
            other => panic!("a ninth key must be counted, not dropped, got {other:?}"),
        }
        assert_eq!(budget.tracked_keys(id), MAX_RECOVERY_KEYS_PER_PANEL);
        assert_eq!(budget.count(id, 3, RecoveryReason::Crashed), 1);
        assert_eq!(
            budget.count(id, 1, RecoveryReason::Suspended),
            0,
            "the stalest key was evicted to make room"
        );
        assert_eq!(budget.records(id).len(), MAX_RECOVERY_RECORDS_PER_PANEL);
        assert_eq!(budget.tracked_panels(), 1);

        // Retirement drops everything for the container.
        budget.retire(id);
        assert_eq!(budget.tracked_panels(), 0);
        assert_eq!(budget.total_records(), 0);
        assert_eq!(budget.tracked_keys(id), 0);
        assert_eq!(budget.count(id, 1, RecoveryReason::Reloaded), 0);
        assert_eq!(budget.saturated_note(id), None);
    }

    /// A `Drop` record is a report, not an attempt: it is not counted, and its
    /// `attempt` is zero so a diagnostic reader never mistakes a disposal for
    /// a recovery attempt. The action is derived from the reason, so `closed`
    /// cannot be recorded as a placeholder.
    #[test]
    fn a_disposal_is_recorded_but_not_counted() {
        let mut budget = RecoveryBudget::new();
        let id = panel(9);
        match budget.note(id, None, 4, RecoveryReason::Closed) {
            RecoveryStep::Recorded(record) => {
                assert_eq!(record.attempt, 0);
                assert_eq!(record.action, RecoveryAction::Disposed);
                assert_eq!(record.reason, RecoveryReason::Closed);
                assert_eq!(
                    record.owner, None,
                    "no binding, so the record names no owner"
                );
            }
            other => panic!("expected a disposal record, got {other:?}"),
        }
        assert_eq!(budget.count(id, 4, RecoveryReason::Closed), 0);
        assert_eq!(budget.tracked_keys(id), 0);
    }

    /// E.5: the surface is the fixed Core table, far inside SCN-1..SCN-5, and
    /// every reason has a distinct detail line so the reason is findable.
    #[test]
    fn the_placeholder_surface_is_fixed_and_bounded() {
        let mut details = Vec::new();
        for reason in PlaceholderReason::ALL {
            let scene = PlaceholderScene::for_reason(reason);
            assert_eq!(scene.node_count(), PLACEHOLDER_SCENE_NODES);
            assert_eq!(scene.depth(), PLACEHOLDER_SCENE_DEPTH);
            assert_eq!(scene.actions().len(), PLACEHOLDER_SCENE_ACTIONS);
            assert_eq!(scene.actions()[0], PlaceholderAction::Close);
            assert_eq!(scene.actions()[1], PlaceholderAction::Retry);
            assert!(
                scene.within_scn_limits(),
                "{reason} must fit the scene limits"
            );
            assert!(
                scene.text_bytes() < 256,
                "{reason} names itself in one line"
            );
            assert_eq!(scene.heading(), "Panel unavailable");
            details.push(scene.detail());
        }
        details.sort_unstable();
        details.dedup();
        assert_eq!(details.len(), PlaceholderReason::ALL.len());
        assert_eq!(
            PlaceholderScene::unavailable().detail(),
            PlaceholderReason::Denied.detail()
        );
    }
}
