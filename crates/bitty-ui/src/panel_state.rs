//! Seven panel state axes (UX-26, CTX-0672).
//!
//! Candidate implementation of U-7
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending Panel RFC). Nothing here is normative,
//! accepted, or verified: every axis name, bound, and rule below is a
//! candidate spelling that the Panel RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! What this module provides:
//!
//! - The seven independent state axes of one panel: [`PanelLifecycle`],
//!   presentation (the accepted [`PresentationMode`](crate::presentation::PresentationMode),
//!   reused, never redefined), [`PanelVisibility`], [`PanelFocusState`],
//!   [`PanelAttention`], [`PanelInteraction`], and [`PanelActivity`].
//! - [`SevenPanelState`] — the seven axes joined to the canonical panel
//!   and tree identities ([`PanelId`](crate::panel::PanelId) and the
//!   canonical [`UiNodeId`](crate::uitree::UiNodeId), reused via import,
//!   never redefined here).
//! - The close rule: [`SevenPanelState::close`] moves the lifecycle axis
//!   to [`PanelLifecycle::Closed`] and hides the panel, but the plugin
//!   stays loaded ([`CloseOutcome::plugin_retained`] is always `true`).
//!   Close never unloads a plugin; unload is the explicit
//!   `Disposed` transition only.
//! - Attention badges ([`PanelAttention::Badge`]): the display count
//!   saturates at [`MAX_BADGE_COUNT`] (`999+`), and badge text is bounded
//!   by [`MAX_BADGE_TEXT_LEN`] characters, rejected with
//!   [`PanelStateError::BadgeTextTooLong`], never silently truncated.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::panel::PanelId;
use crate::presentation::PresentationMode;
use crate::uitree::UiNodeId;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on panels carrying seven-axis state in one registry.
///
/// Rejected with [`PanelStateError::TooManyPanels`], never silently pruned.
pub const MAX_PANELS_WITH_STATE: usize = 256;

/// Display saturation for attention badge counts.
///
/// The stored count keeps its full value; only the rendered badge text
/// saturates (see [`PanelAttention::display_count`]).
pub const MAX_BADGE_COUNT: u32 = 999;

/// Hard cap in characters for attention badge text.
///
/// Rejected with [`PanelStateError::BadgeTextTooLong`], never silently
/// truncated: truncation would mislabel the surface.
pub const MAX_BADGE_TEXT_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to evolve seven-axis panel state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelStateError {
    /// The lifecycle transition `from -> to` is not allowed.
    IllegalTransition {
        /// Transition source.
        from: PanelLifecycle,
        /// Rejected transition target.
        to: PanelLifecycle,
    },
    /// Badge text exceeds [`MAX_BADGE_TEXT_LEN`] characters.
    BadgeTextTooLong {
        /// Characters counted in the submitted text.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The lifecycle axis is already [`PanelLifecycle::Disposed`];
    /// a disposed panel accepts no further transition.
    AlreadyDisposed,
    /// More than [`MAX_PANELS_WITH_STATE`] panels were submitted.
    TooManyPanels {
        /// Panels counted in the submitted batch.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
}

impl fmt::Display for PanelStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IllegalTransition { from, to } => {
                write!(f, "illegal lifecycle transition {from} -> {to}")
            }
            Self::BadgeTextTooLong { found, cap } => {
                write!(f, "badge text too long: {found} chars, cap {cap}")
            }
            Self::AlreadyDisposed => f.write_str("panel already disposed"),
            Self::TooManyPanels { found, cap } => {
                write!(f, "too many panels: {found}, cap {cap}")
            }
        }
    }
}

impl std::error::Error for PanelStateError {}

// ---------------------------------------------------------------------------
// Axis 1: Lifecycle (close never unloads the plugin)
// ---------------------------------------------------------------------------

/// Lifecycle axis of one panel (UX-26 axis 1).
///
/// Mirrors the accepted [`PanelState`](crate::panel::PanelState) vocabulary
/// with one addition: [`Closed`](Self::Closed). Closing a panel parks it
/// (hidden, unfocused, plugin still loaded); only the explicit transition
/// to [`Disposed`](Self::Disposed) releases the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelLifecycle {
    /// Manifest-declared but not yet allocated.
    Declared,
    /// Allocated but not mounted.
    Created,
    /// Bound to a view.
    Mounted,
    /// Mounted and owning input routing.
    Focused,
    /// Invisible without destroying attachment.
    Suspended,
    /// Closed by the user: hidden and unfocused, plugin still loaded.
    Closed,
    /// All resources released; the plugin is unloaded.
    Disposed,
}

impl PanelLifecycle {
    /// Whether transition `from -> to` is allowed.
    #[must_use]
    pub fn can_transition(from: Self, to: Self) -> bool {
        if from == to {
            return true;
        }
        matches!(
            (from, to),
            (Self::Declared, Self::Created)
                | (Self::Created, Self::Mounted)
                | (Self::Mounted, Self::Focused)
                | (Self::Mounted, Self::Suspended)
                | (Self::Focused, Self::Mounted)
                | (Self::Focused, Self::Suspended)
                | (Self::Suspended, Self::Mounted)
                | (Self::Closed, Self::Mounted)
                | (Self::Suspended, Self::Disposed)
                | (Self::Closed, Self::Disposed)
                | (Self::Mounted, Self::Disposed)
        )
    }

    /// Close is allowed from every state except [`Disposed`](Self::Disposed).
    #[must_use]
    pub fn can_close(from: Self) -> bool {
        !matches!(from, Self::Disposed)
    }

    /// Parses the canonical snapshot spelling (lowercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "declared" => Some(Self::Declared),
            "created" => Some(Self::Created),
            "mounted" => Some(Self::Mounted),
            "focused" => Some(Self::Focused),
            "suspended" => Some(Self::Suspended),
            "closed" => Some(Self::Closed),
            "disposed" => Some(Self::Disposed),
            _ => None,
        }
    }

    /// The canonical snapshot spelling (lowercase).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Created => "created",
            Self::Mounted => "mounted",
            Self::Focused => "focused",
            Self::Suspended => "suspended",
            Self::Closed => "closed",
            Self::Disposed => "disposed",
        }
    }
}

impl fmt::Display for PanelLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Axis 3: Visibility (presentation stays the accepted PresentationMode)
// ---------------------------------------------------------------------------

/// Visibility axis of one panel (UX-26 axis 3).
///
/// Deliberately distinct from presentation: presentation says *how* a panel
/// would draw, visibility says whether it currently shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelVisibility {
    /// Currently shown.
    Visible,
    /// Hidden by the user, the workspace, or close.
    Hidden,
    /// Covered by an overlay or floating panel.
    Occluded,
    /// Minimized to a task strip.
    Minimized,
}

impl PanelVisibility {
    /// Parses the canonical snapshot spelling (lowercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "visible" => Some(Self::Visible),
            "hidden" => Some(Self::Hidden),
            "occluded" => Some(Self::Occluded),
            "minimized" => Some(Self::Minimized),
            _ => None,
        }
    }

    /// The canonical snapshot spelling (lowercase).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Occluded => "occluded",
            Self::Minimized => "minimized",
        }
    }
}

impl fmt::Display for PanelVisibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Axis 4: Focus
// ---------------------------------------------------------------------------

/// Focus axis of one panel (UX-26 axis 4).
///
/// The single-panel projection of the crate [`PanelFocus`](crate::panel::PanelFocus)
/// registry: at most one panel per scope reports [`Focused`](Self::Focused).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelFocusState {
    /// Owning keyboard/IME/wheel routing in its scope.
    Focused,
    /// Not owning routing.
    Unfocused,
}

impl PanelFocusState {
    /// Parses the canonical snapshot spelling (lowercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "focused" => Some(Self::Focused),
            "unfocused" => Some(Self::Unfocused),
            _ => None,
        }
    }

    /// The canonical snapshot spelling (lowercase).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::Unfocused => "unfocused",
        }
    }
}

impl fmt::Display for PanelFocusState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Axis 5: Attention (badges)
// ---------------------------------------------------------------------------

/// Attention axis of one panel (UX-26 axis 5).
///
/// Summons the user back to a background panel. [`Badge`](Self::Badge)
/// carries a raw count plus a short label; rendering saturates the count
/// at [`MAX_BADGE_COUNT`] (see [`display_count`](Self::display_count)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelAttention {
    /// Nothing needs the user.
    Quiet,
    /// Counted badge with a short label (for example `3` + `"msgs"`).
    Badge {
        /// Raw event count (never saturates in storage).
        count: u32,
        /// Short label, at most [`MAX_BADGE_TEXT_LEN`] characters.
        text: String,
    },
    /// Uncounted glow (for example activity while visible elsewhere).
    Highlight,
    /// Demands immediate focus (for example a blocking prompt).
    Urgent,
}

impl PanelAttention {
    /// Builds a [`Badge`](Self::Badge), bounding the label.
    pub fn badge(count: u32, text: &str) -> Result<Self, PanelStateError> {
        let found = text.chars().count();
        if found > MAX_BADGE_TEXT_LEN {
            return Err(PanelStateError::BadgeTextTooLong {
                found,
                cap: MAX_BADGE_TEXT_LEN,
            });
        }
        Ok(Self::Badge {
            count,
            text: text.to_owned(),
        })
    }

    /// The renderable count: saturates at [`MAX_BADGE_COUNT`].
    #[must_use]
    pub fn display_count(&self) -> Option<u32> {
        match self {
            Self::Badge { count, .. } => {
                if *count > MAX_BADGE_COUNT {
                    Some(MAX_BADGE_COUNT)
                } else {
                    Some(*count)
                }
            }
            Self::Quiet | Self::Highlight | Self::Urgent => None,
        }
    }

    /// Whether the badge display saturates (`count > MAX_BADGE_COUNT`).
    #[must_use]
    pub const fn is_saturated(&self) -> bool {
        match self {
            Self::Badge { count, .. } => *count > MAX_BADGE_COUNT,
            Self::Quiet | Self::Highlight | Self::Urgent => false,
        }
    }

    /// The canonical snapshot kind spelling (lowercase).
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Badge { .. } => "badge",
            Self::Highlight => "highlight",
            Self::Urgent => "urgent",
        }
    }
}

// ---------------------------------------------------------------------------
// Axis 6: Interaction
// ---------------------------------------------------------------------------

/// Interaction axis of one panel (UX-26 axis 6): the transient pointer or
/// key gesture in progress, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelInteraction {
    /// No gesture in progress.
    Idle,
    /// Pointer hovering, nothing pressed.
    Hovered,
    /// Pointer or key pressed, gesture uncommitted.
    Pressed,
    /// A move gesture is driving the panel.
    Dragging,
    /// Text is being composed into the panel.
    Typing,
}

impl PanelInteraction {
    /// Parses the canonical snapshot spelling (lowercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "hovered" => Some(Self::Hovered),
            "pressed" => Some(Self::Pressed),
            "dragging" => Some(Self::Dragging),
            "typing" => Some(Self::Typing),
            _ => None,
        }
    }

    /// The canonical snapshot spelling (lowercase).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Hovered => "hovered",
            Self::Pressed => "pressed",
            Self::Dragging => "dragging",
            Self::Typing => "typing",
        }
    }
}

impl fmt::Display for PanelInteraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Axis 7: Activity
// ---------------------------------------------------------------------------

/// Activity axis of one panel (UX-26 axis 7): whether the panel's content
/// is live, quiet, parked, or gone stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelActivity {
    /// Producing or consuming events.
    Active,
    /// Mounted but quiet.
    Idle,
    /// Parked (for example on an inactive workspace); cheap to resume.
    Suspended,
    /// Its content source went away; re-open re-binds.
    Stale,
}

impl PanelActivity {
    /// Parses the canonical snapshot spelling (lowercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "idle" => Some(Self::Idle),
            "suspended" => Some(Self::Suspended),
            "stale" => Some(Self::Stale),
            _ => None,
        }
    }

    /// The canonical snapshot spelling (lowercase).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Suspended => "suspended",
            Self::Stale => "stale",
        }
    }
}

impl fmt::Display for PanelActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Joined state + close outcome
// ---------------------------------------------------------------------------

/// Outcome of [`SevenPanelState::close`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseOutcome {
    /// Always `true`: close parks the panel, it never unloads the plugin.
    pub plugin_retained: bool,
    /// Always `true` unless the panel was already disposed: a closed panel
    /// can transition back to [`PanelLifecycle::Mounted`].
    pub reopenable: bool,
}

/// The seven state axes of one panel, joined to panel and tree identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenPanelState {
    panel: PanelId,
    node: UiNodeId,
    lifecycle: PanelLifecycle,
    presentation: PresentationMode,
    visibility: PanelVisibility,
    focus: PanelFocusState,
    attention: PanelAttention,
    interaction: PanelInteraction,
    activity: PanelActivity,
}

impl SevenPanelState {
    /// Fresh parked state: declared, tiled, hidden, unfocused, quiet,
    /// idle, suspended.
    #[must_use]
    pub const fn new(panel: PanelId, node: UiNodeId) -> Self {
        Self {
            panel,
            node,
            lifecycle: PanelLifecycle::Declared,
            presentation: PresentationMode::Tiled,
            visibility: PanelVisibility::Hidden,
            focus: PanelFocusState::Unfocused,
            attention: PanelAttention::Quiet,
            interaction: PanelInteraction::Idle,
            activity: PanelActivity::Suspended,
        }
    }

    #[must_use]
    pub const fn panel(&self) -> PanelId {
        self.panel
    }

    /// The canonical [`UiNodeId`](crate::uitree::UiNodeId) of the panel's
    /// tree node (same type the retained tree diffs by, never a copy).
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    #[must_use]
    pub const fn lifecycle(&self) -> PanelLifecycle {
        self.lifecycle
    }

    #[must_use]
    pub const fn presentation(&self) -> PresentationMode {
        self.presentation
    }

    #[must_use]
    pub const fn visibility(&self) -> PanelVisibility {
        self.visibility
    }

    #[must_use]
    pub const fn focus(&self) -> PanelFocusState {
        self.focus
    }

    #[must_use]
    pub const fn attention(&self) -> &PanelAttention {
        &self.attention
    }

    #[must_use]
    pub const fn interaction(&self) -> PanelInteraction {
        self.interaction
    }

    #[must_use]
    pub const fn activity(&self) -> PanelActivity {
        self.activity
    }

    /// Steps the lifecycle axis through the transition gate.
    pub fn set_lifecycle(&mut self, to: PanelLifecycle) -> Result<(), PanelStateError> {
        if self.lifecycle == PanelLifecycle::Disposed {
            return Err(PanelStateError::AlreadyDisposed);
        }
        if PanelLifecycle::can_transition(self.lifecycle, to) {
            self.lifecycle = to;
            Ok(())
        } else {
            Err(PanelStateError::IllegalTransition {
                from: self.lifecycle,
                to,
            })
        }
    }

    /// Sets the presentation axis (axis 2 reuses the accepted mode type).
    pub fn set_presentation(&mut self, mode: PresentationMode) {
        self.presentation = mode;
    }

    /// Sets the visibility axis.
    pub fn set_visibility(&mut self, visibility: PanelVisibility) {
        self.visibility = visibility;
    }

    /// Sets the attention axis (badge labels stay bounded).
    pub fn set_attention(&mut self, attention: PanelAttention) {
        self.attention = attention;
    }

    /// Sets the interaction axis.
    pub fn set_interaction(&mut self, interaction: PanelInteraction) {
        self.interaction = interaction;
    }

    /// Sets the activity axis.
    pub fn set_activity(&mut self, activity: PanelActivity) {
        self.activity = activity;
    }

    /// Focuses the panel: lifecycle `Mounted -> Focused` plus the focus
    /// axis, so the two never disagree through this path.
    pub fn focus_panel(&mut self) -> Result<(), PanelStateError> {
        if self.lifecycle == PanelLifecycle::Mounted {
            self.set_lifecycle(PanelLifecycle::Focused)?;
        } else if self.lifecycle != PanelLifecycle::Focused {
            return Err(PanelStateError::IllegalTransition {
                from: self.lifecycle,
                to: PanelLifecycle::Focused,
            });
        }
        self.focus = PanelFocusState::Focused;
        Ok(())
    }

    /// Unfocuses without parking: lifecycle `Focused -> Mounted`.
    pub fn unfocus_panel(&mut self) -> Result<(), PanelStateError> {
        if self.lifecycle == PanelLifecycle::Focused {
            self.set_lifecycle(PanelLifecycle::Mounted)?;
        }
        self.focus = PanelFocusState::Unfocused;
        Ok(())
    }

    /// Closes the panel: lifecycle to `Closed`, visibility to `Hidden`,
    /// focus to `Unfocused`. Attention survives (badges still count on a
    /// closed panel) and the plugin stays loaded.
    pub fn close(&mut self) -> Result<CloseOutcome, PanelStateError> {
        if self.lifecycle == PanelLifecycle::Disposed {
            return Err(PanelStateError::AlreadyDisposed);
        }
        self.lifecycle = PanelLifecycle::Closed;
        self.visibility = PanelVisibility::Hidden;
        self.focus = PanelFocusState::Unfocused;
        self.interaction = PanelInteraction::Idle;
        Ok(CloseOutcome {
            plugin_retained: true,
            reopenable: true,
        })
    }

    /// Reopens a closed panel back to `Mounted` and visible.
    pub fn reopen(&mut self) -> Result<(), PanelStateError> {
        self.set_lifecycle(PanelLifecycle::Mounted)?;
        self.visibility = PanelVisibility::Visible;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SevenPanelState {
        SevenPanelState::new(PanelId::new(1), UiNodeId::new(2))
    }

    #[test]
    fn close_parks_but_retains_plugin() {
        let mut s = state();
        s.set_lifecycle(PanelLifecycle::Created).unwrap();
        s.set_lifecycle(PanelLifecycle::Mounted).unwrap();
        s.focus_panel().unwrap();
        let out = s.close().unwrap();
        assert!(out.plugin_retained);
        assert!(out.reopenable);
        assert_eq!(s.lifecycle(), PanelLifecycle::Closed);
        assert_eq!(s.visibility(), PanelVisibility::Hidden);
        assert_eq!(s.focus(), PanelFocusState::Unfocused);
        s.reopen().unwrap();
        assert_eq!(s.lifecycle(), PanelLifecycle::Mounted);
    }

    #[test]
    fn disposed_rejects_close_and_transitions() {
        let mut s = state();
        s.set_lifecycle(PanelLifecycle::Created).unwrap();
        s.set_lifecycle(PanelLifecycle::Mounted).unwrap();
        s.set_lifecycle(PanelLifecycle::Disposed).unwrap();
        assert_eq!(s.close(), Err(PanelStateError::AlreadyDisposed));
    }

    #[test]
    fn badge_text_bound_rejects() {
        let long = "x".repeat(MAX_BADGE_TEXT_LEN + 1);
        assert!(PanelAttention::badge(1, &long).is_err());
        let ok = "x".repeat(MAX_BADGE_TEXT_LEN);
        assert!(PanelAttention::badge(1, &ok).is_ok());
        let big = PanelAttention::badge(5000, "msgs").unwrap();
        assert_eq!(big.display_count(), Some(MAX_BADGE_COUNT));
        assert!(big.is_saturated());
    }
}
