//! Typed panel rules: declarative placement, presentation, size, and
//! appearance rules with priority, specificity, and conflict diagnostics
//! (UX-19, CTX-0669).
//!
//! Candidate implementation of U-4 panel rules
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every selector name, origin rank, bound, and rule
//! below is a candidate spelling that the UI Runtime RFC accepts or rejects,
//! never this module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`PanelRule`] — one typed declaration: a [`RuleSelector`] (which
//!   panels), a [`RuleEffect`] (what is claimed), and a [`RuleOrigin`]
//!   (who claims it). Effects reuse the crate vocabulary instead of
//!   inventing a parallel one: [`SceneLayer`](crate::workspace_scene::SceneLayer)
//!   for placement, [`PresentationMode`](crate::presentation::PresentationMode)
//!   for presentation, [`Size`](crate::geometry::Size) for minimum size, and
//!   [`TokenColor`](crate::theme::TokenColor) for the accent.
//! - [`PanelRuleSet`] — a bounded rule collection with fail-closed
//!   admission (full set, duplicate rule id) and deterministic resolution.
//! - Resolution — for one panel and one [`EffectKind`], the winner is the
//!   highest [`RuleOrigin`] rank, then the narrowest [`RuleSelector`]
//!   specificity, then the lowest [`RuleId`]. An exact tie on origin and
//!   specificity is not guessed: resolution fails closed with
//!   [`RuleError::Conflict`] carrying a [`RuleDiagnostic`] that names every
//!   contender.
//!
//! Relationship to the style cascade: an accent effect joins the candidate
//! [`ResolvedStyle`](crate::resolved_style::ResolvedStyle) cascade at the
//! rule's origin rank (`Safety` above `User` above `Workspace`), matching
//! the UX-20 order where user rules beat workspace rules. Rules never carry
//! a window handle and never mutate terminal state.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, or platform handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::geometry::Size;
use crate::panel::{PanelId, PanelType};
use crate::presentation::PresentationMode;
use crate::theme::TokenColor;
use crate::workspace_scene::SceneLayer;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on rules per set.
///
/// Rejected with [`RuleError::TooManyRules`], never silently pruned: pruning
/// would present a partial rule set as complete.
pub const MAX_PANEL_RULES: usize = 64;

// ---------------------------------------------------------------------------
// Errors and diagnostics
// ---------------------------------------------------------------------------

/// Stable handle for one panel rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleId(pub u64);

impl RuleId {
    /// Creates an id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RuleId({})", self.0)
    }
}

/// One claimed effect kind, used to scope resolution and conflicts.
///
/// Each kind resolves independently: a placement tie never suppresses a
/// presentation winner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EffectKind {
    Placement,
    Presentation,
    MinSize,
    Accent,
}

impl EffectKind {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Placement => "placement",
            Self::Presentation => "presentation",
            Self::MinSize => "min-size",
            Self::Accent => "accent",
        }
    }
}

impl fmt::Display for EffectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Conflict diagnostic: the effect kind under contention and every tied
/// contender in deterministic ([`RuleId`]) order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleDiagnostic {
    /// The contested effect kind.
    pub kind: EffectKind,
    /// The tied rule ids, sorted ascending.
    pub contenders: Vec<RuleId>,
}

impl RuleDiagnostic {
    /// Builds a diagnostic with contenders in deterministic order.
    #[must_use]
    pub fn new(kind: EffectKind, mut contenders: Vec<RuleId>) -> Self {
        contenders.sort();
        Self { kind, contenders }
    }
}

impl fmt::Display for RuleDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conflict on {}: rules ", self.kind)?;
        let mut first = true;
        for id in &self.contenders {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{id}")?;
        }
        Ok(())
    }
}

/// Failure to admit or resolve a panel rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// The set holds [`MAX_PANEL_RULES`] rules already.
    TooManyRules {
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Two rules share one [`RuleId`].
    DuplicateRule {
        /// The repeated identifier.
        id: RuleId,
    },
    /// Two rules tie on origin rank and specificity for one panel and one
    /// effect kind. The diagnostic names every contender; the caller
    /// narrows a selector or retires a rule.
    Conflict(RuleDiagnostic),
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyRules { cap } => {
                write!(f, "panel rule set full: cap {cap}")
            }
            Self::DuplicateRule { id } => write!(f, "duplicate panel rule id: {id}"),
            Self::Conflict(diagnostic) => write!(f, "{diagnostic}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Origin, selector, effect
// ---------------------------------------------------------------------------

/// Who claims a rule. Rank decides first: safety beats user beats
/// workspace, mirroring the UX-20 cascade order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuleOrigin {
    /// Workspace-level rule (lowest rank).
    Workspace,
    /// User-level rule.
    User,
    /// Safety policy (highest rank, never overridden by user rules).
    Safety,
}

impl RuleOrigin {
    /// Priority rank: higher wins.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Workspace => 0,
            Self::User => 1,
            Self::Safety => 2,
        }
    }

    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::User => "user",
            Self::Safety => "safety",
        }
    }

    /// Parses a canonical [`Self::as_str`] name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(Self::Workspace),
            "user" => Some(Self::User),
            "safety" => Some(Self::Safety),
            _ => None,
        }
    }
}

impl fmt::Display for RuleOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which panels a rule applies to. Specificity decides after origin rank:
/// one panel beats one panel type beats every panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuleSelector {
    /// Every panel.
    AnyPanel,
    /// Panels of one type.
    PanelType(PanelType),
    /// Exactly one panel.
    Panel(PanelId),
}

impl RuleSelector {
    /// Specificity rank: higher wins after origin rank.
    #[must_use]
    pub const fn specificity(self) -> u8 {
        match self {
            Self::AnyPanel => 0,
            Self::PanelType(_) => 1,
            Self::Panel(_) => 2,
        }
    }

    /// Reports whether the selector covers this panel.
    #[must_use]
    pub fn matches(self, panel: PanelId, panel_type: PanelType) -> bool {
        match self {
            Self::AnyPanel => true,
            Self::PanelType(kind) => kind == panel_type,
            Self::Panel(id) => id.0 == panel.0,
        }
    }
}

/// The claimed value. One rule carries exactly one effect; a rule that sets
/// several properties is written as several rules sharing an origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuleEffect {
    /// Scene layer placement.
    Placement(SceneLayer),
    /// Per-leaf display mode.
    Presentation(PresentationMode),
    /// Minimum panel size in cells.
    MinSize(Size),
    /// Panel accent color.
    Accent(TokenColor),
}

impl RuleEffect {
    /// The effect kind this value claims.
    #[must_use]
    pub const fn kind(self) -> EffectKind {
        match self {
            Self::Placement(_) => EffectKind::Placement,
            Self::Presentation(_) => EffectKind::Presentation,
            Self::MinSize(_) => EffectKind::MinSize,
            Self::Accent(_) => EffectKind::Accent,
        }
    }
}

/// One typed panel rule: identity, origin, selector, and a single effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PanelRule {
    id: RuleId,
    origin: RuleOrigin,
    selector: RuleSelector,
    effect: RuleEffect,
}

impl PanelRule {
    /// Builds a rule. No validation beyond construction: admission checks
    /// live on [`PanelRuleSet::add`].
    #[must_use]
    pub const fn new(
        id: RuleId,
        origin: RuleOrigin,
        selector: RuleSelector,
        effect: RuleEffect,
    ) -> Self {
        Self {
            id,
            origin,
            selector,
            effect,
        }
    }

    /// Returns the rule identity.
    #[must_use]
    pub const fn id(&self) -> RuleId {
        self.id
    }

    /// Returns the rule origin.
    #[must_use]
    pub const fn origin(&self) -> RuleOrigin {
        self.origin
    }

    /// Returns the rule selector.
    #[must_use]
    pub const fn selector(&self) -> RuleSelector {
        self.selector
    }

    /// Returns the rule effect.
    #[must_use]
    pub const fn effect(&self) -> RuleEffect {
        self.effect
    }
}

// ---------------------------------------------------------------------------
// Rule set and resolution
// ---------------------------------------------------------------------------

/// A bounded panel rule collection with deterministic resolution.
///
/// Rules resolve per [`EffectKind`]: for one panel and one kind, the winner
/// is the highest origin rank, then the narrowest selector, then the lowest
/// rule id. An exact tie on origin and specificity fails closed with
/// [`RuleError::Conflict`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelRuleSet {
    rules: Vec<PanelRule>,
}

impl PanelRuleSet {
    /// Builds an empty rule set.
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Builds a set from a rule list, admitting in order fail-closed.
    pub fn from_rules(rules: Vec<PanelRule>) -> Result<Self, RuleError> {
        let mut set = Self::new();
        for rule in rules {
            set.add(rule)?;
        }
        Ok(set)
    }

    /// Admits a rule, rejecting a full set or a duplicate id fail-closed.
    pub fn add(&mut self, rule: PanelRule) -> Result<(), RuleError> {
        if self.rules.len() >= MAX_PANEL_RULES {
            return Err(RuleError::TooManyRules {
                cap: MAX_PANEL_RULES,
            });
        }
        if self.rules.iter().any(|r| r.id == rule.id) {
            return Err(RuleError::DuplicateRule { id: rule.id });
        }
        self.rules.push(rule);
        Ok(())
    }

    /// Returns admitted rules in admission order.
    #[must_use]
    pub fn rules(&self) -> &[PanelRule] {
        &self.rules
    }

    /// Resolves one effect kind for one panel.
    ///
    /// Returns `Ok(None)` when no rule claims the kind for this panel, and
    /// `Err(Conflict)` when the top contenders tie on origin and
    /// specificity.
    pub fn resolve(
        &self,
        panel: PanelId,
        panel_type: PanelType,
        kind: EffectKind,
    ) -> Result<Option<PanelRule>, RuleError> {
        let mut best: Option<&PanelRule> = None;
        let mut tied: Vec<RuleId> = Vec::new();
        for rule in &self.rules {
            if rule.effect.kind() != kind {
                continue;
            }
            if !rule.selector.matches(panel, panel_type) {
                continue;
            }
            match best {
                None => {
                    best = Some(rule);
                    tied.clear();
                    tied.push(rule.id);
                }
                Some(current) => {
                    let rank = (rule.origin.rank(), rule.selector.specificity(), rule.id);
                    let current_rank = (
                        current.origin.rank(),
                        current.selector.specificity(),
                        current.id,
                    );
                    if rank.0 > current_rank.0
                        || (rank.0 == current_rank.0 && rank.1 > current_rank.1)
                    {
                        best = Some(rule);
                        tied.clear();
                        tied.push(rule.id);
                    } else if rank.0 == current_rank.0 && rank.1 == current_rank.1 {
                        tied.push(rule.id);
                    }
                }
            }
        }
        match best {
            None => Ok(None),
            Some(winner) => {
                if tied.len() > 1 {
                    Err(RuleError::Conflict(RuleDiagnostic::new(kind, tied)))
                } else {
                    Ok(Some(*winner))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> PanelRuleSet {
        PanelRuleSet::new()
    }

    #[test]
    fn origin_rank_beats_specificity() {
        let mut rules = set();
        // A broad safety rule beats a narrow workspace rule.
        rules
            .add(PanelRule::new(
                RuleId::new(1),
                RuleOrigin::Safety,
                RuleSelector::AnyPanel,
                RuleEffect::Presentation(PresentationMode::Tiled),
            ))
            .expect("fits");
        rules
            .add(PanelRule::new(
                RuleId::new(2),
                RuleOrigin::Workspace,
                RuleSelector::Panel(PanelId::new(9)),
                RuleEffect::Presentation(PresentationMode::Floating),
            ))
            .expect("fits");
        let winner = rules
            .resolve(
                PanelId::new(9),
                PanelType::Terminal,
                EffectKind::Presentation,
            )
            .expect("resolves")
            .expect("claimed");
        assert_eq!(winner.id(), RuleId::new(1));
    }

    #[test]
    fn specificity_breaks_origin_ties() {
        let mut rules = set();
        rules
            .add(PanelRule::new(
                RuleId::new(1),
                RuleOrigin::User,
                RuleSelector::AnyPanel,
                RuleEffect::Placement(SceneLayer::Tiled),
            ))
            .expect("fits");
        rules
            .add(PanelRule::new(
                RuleId::new(2),
                RuleOrigin::User,
                RuleSelector::PanelType(PanelType::Browser),
                RuleEffect::Placement(SceneLayer::Floating),
            ))
            .expect("fits");
        let winner = rules
            .resolve(PanelId::new(4), PanelType::Browser, EffectKind::Placement)
            .expect("resolves")
            .expect("claimed");
        assert_eq!(winner.id(), RuleId::new(2));
        // A terminal panel still sees the broad rule.
        let fallback = rules
            .resolve(PanelId::new(5), PanelType::Terminal, EffectKind::Placement)
            .expect("resolves")
            .expect("claimed");
        assert_eq!(fallback.id(), RuleId::new(1));
    }

    #[test]
    fn exact_tie_is_a_conflict_with_named_contenders() {
        let mut rules = set();
        for id in [1u64, 2] {
            rules
                .add(PanelRule::new(
                    RuleId::new(id),
                    RuleOrigin::User,
                    RuleSelector::PanelType(PanelType::Helper),
                    RuleEffect::MinSize(Size::new(20, 6)),
                ))
                .expect("fits");
        }
        let err = rules
            .resolve(PanelId::new(3), PanelType::Helper, EffectKind::MinSize)
            .expect_err("tie must fail closed");
        match err {
            RuleError::Conflict(diagnostic) => {
                assert_eq!(diagnostic.kind, EffectKind::MinSize);
                assert_eq!(diagnostic.contenders, vec![RuleId::new(1), RuleId::new(2)]);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn kinds_resolve_independently_and_empty_resolves_none() {
        let mut rules = set();
        rules
            .add(PanelRule::new(
                RuleId::new(1),
                RuleOrigin::Workspace,
                RuleSelector::AnyPanel,
                RuleEffect::Accent(TokenColor::from_rgba([0x11, 0x22, 0x33, 0xFF])),
            ))
            .expect("fits");
        assert!(
            rules
                .resolve(PanelId::new(1), PanelType::Rich, EffectKind::Placement)
                .expect("resolves")
                .is_none()
        );
        assert!(
            rules
                .resolve(PanelId::new(1), PanelType::Rich, EffectKind::Accent)
                .expect("resolves")
                .is_some()
        );
    }

    #[test]
    fn admission_is_bounded_and_deduped() {
        let mut rules = set();
        for n in 0..MAX_PANEL_RULES {
            rules
                .add(PanelRule::new(
                    RuleId::new(n as u64),
                    RuleOrigin::Workspace,
                    RuleSelector::AnyPanel,
                    RuleEffect::Placement(SceneLayer::Tiled),
                ))
                .expect("fits");
        }
        assert!(matches!(
            rules.add(PanelRule::new(
                RuleId::new(10_000),
                RuleOrigin::User,
                RuleSelector::AnyPanel,
                RuleEffect::Placement(SceneLayer::Pinned),
            )),
            Err(RuleError::TooManyRules { .. })
        ));
        let mut small = set();
        small
            .add(PanelRule::new(
                RuleId::new(7),
                RuleOrigin::User,
                RuleSelector::AnyPanel,
                RuleEffect::Placement(SceneLayer::Tiled),
            ))
            .expect("fits");
        assert_eq!(
            small.add(PanelRule::new(
                RuleId::new(7),
                RuleOrigin::Safety,
                RuleSelector::AnyPanel,
                RuleEffect::Placement(SceneLayer::Pinned),
            )),
            Err(RuleError::DuplicateRule { id: RuleId::new(7) })
        );
    }
}
