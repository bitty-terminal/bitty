//! Window chrome runtime: five headless chrome surfaces (UX-18, CTX-0669).
//!
//! Candidate implementation of U-4 chrome
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every surface name, bound, and rule below is a
//! candidate spelling that the UI Runtime RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`ChromeSurface`] — the five named chrome surfaces of a window
//!   (`WorkspaceRail`, `StatusBar`, `OverlayRoot`, `NotificationArea`,
//!   `CommandSurface`). Names are stable lowercase spellings with
//!   [`ChromeSurface::parse`]; there is no sixth surface by construction.
//! - [`WindowChromeRuntime`] — headless per-window chrome state: per-surface
//!   visibility, a bounded notification queue ([`ChromeNotification`]),
//!   a single command surface session bound to one [`UiNodeId`], and the
//!   overlay-root attachment set over [`UiNodeId`]. All mutations are
//!   fail-closed and deterministic.
//! - [`ChromeError`] — the rejection vocabulary (full queue, duplicate or
//!   unknown identities, busy command surface).
//!
//! Relationship to the accepted models: chrome surfaces compose **beside**
//! the [`WorkspaceScene`](crate::workspace_scene::WorkspaceScene) layers and
//! the retained [`UiTree`](crate::uitree::UiTree). The overlay root hosts
//! [`UiNodeId`] presentation attachments only (imported from the canonical
//! [`uitree`](crate::uitree) source, never redefined here); it carries no
//! terminal handle and cannot mutate grid, cursor, modes, or scrollback.
//! No value here exposes a window handle, native surface, or OS window
//! identifier.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, or platform handle participates; notification order is
//! insertion order and overlay membership reports are sorted by node id.

#![forbid(unsafe_code)]

use std::fmt;

use crate::uitree::UiNodeId;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on queued notifications per window.
///
/// Rejected with [`ChromeError::NotificationsFull`], never silently dropped:
/// a dropped notice would present a quiet surface as healthy.
pub const MAX_NOTIFICATIONS: usize = 16;

/// Hard cap in characters for a notification body.
///
/// Rejected with [`ChromeError::TextTooLong`], never silently truncated:
/// truncation would mislabel the notice.
pub const MAX_NOTIFICATION_TEXT_LEN: usize = 256;

/// Hard cap on overlay-root attachments per window.
///
/// Rejected with [`ChromeError::OverlayRootFull`].
pub const MAX_OVERLAY_NODES: usize = 8;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to mutate window chrome state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChromeError {
    /// The notification queue holds [`MAX_NOTIFICATIONS`] entries already.
    NotificationsFull {
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A notification body exceeds [`MAX_NOTIFICATION_TEXT_LEN`].
    TextTooLong {
        /// Length in characters of the rejected body.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Two queued notifications share one [`NotificationId`].
    DuplicateNotification {
        /// The repeated identifier.
        id: NotificationId,
    },
    /// No queued notification carries this [`NotificationId`].
    UnknownNotification {
        /// The unrecognized identifier.
        id: NotificationId,
    },
    /// The command surface already hosts an open session.
    CommandBusy,
    /// The command surface holds no open session.
    CommandClosed,
    /// The overlay root hosts [`MAX_OVERLAY_NODES`] nodes already.
    OverlayRootFull {
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The node is already attached to the overlay root.
    DuplicateOverlayNode {
        /// The repeated node.
        id: UiNodeId,
    },
    /// The node is not attached to the overlay root.
    UnknownOverlayNode {
        /// The unrecognized node.
        id: UiNodeId,
    },
}

impl fmt::Display for ChromeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotificationsFull { cap } => {
                write!(f, "notification queue full: cap {cap}")
            }
            Self::TextTooLong { len, cap } => {
                write!(f, "notification too long: {len} chars exceeds cap {cap}")
            }
            Self::DuplicateNotification { id } => {
                write!(f, "duplicate notification id: {id}")
            }
            Self::UnknownNotification { id } => {
                write!(f, "unknown notification id: {id}")
            }
            Self::CommandBusy => write!(f, "command surface already open"),
            Self::CommandClosed => write!(f, "command surface is closed"),
            Self::OverlayRootFull { cap } => {
                write!(f, "overlay root full: cap {cap}")
            }
            Self::DuplicateOverlayNode { id } => {
                write!(f, "duplicate overlay node: {id}")
            }
            Self::UnknownOverlayNode { id } => {
                write!(f, "unknown overlay node: {id}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

/// The five named chrome surfaces of a window.
///
/// Closed set: matching is exhaustive, so a sixth surface cannot appear by
/// accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChromeSurface {
    /// Panel/project switcher rail.
    WorkspaceRail,
    /// Bottom status line.
    StatusBar,
    /// Host for floating chrome attachments ([`UiNodeId`]).
    OverlayRoot,
    /// Queued notices.
    NotificationArea,
    /// Palette / command entry point (single session).
    CommandSurface,
}

impl ChromeSurface {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRail => "workspace-rail",
            Self::StatusBar => "status-bar",
            Self::OverlayRoot => "overlay-root",
            Self::NotificationArea => "notification-area",
            Self::CommandSurface => "command-surface",
        }
    }

    /// Parses a canonical [`Self::as_str`] name. Case-sensitive; rejects
    /// empty, whitespace-padded, and unknown inputs with no silent aliasing.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace-rail" => Some(Self::WorkspaceRail),
            "status-bar" => Some(Self::StatusBar),
            "overlay-root" => Some(Self::OverlayRoot),
            "notification-area" => Some(Self::NotificationArea),
            "command-surface" => Some(Self::CommandSurface),
            _ => None,
        }
    }

    /// Index into the per-surface visibility table.
    const fn index(self) -> usize {
        match self {
            Self::WorkspaceRail => 0,
            Self::StatusBar => 1,
            Self::OverlayRoot => 2,
            Self::NotificationArea => 3,
            Self::CommandSurface => 4,
        }
    }
}

impl fmt::Display for ChromeSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// Stable handle for a queued chrome notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NotificationId(pub u64);

impl NotificationId {
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

impl fmt::Display for NotificationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NotificationId({})", self.0)
    }
}

/// Notice severity. Presentation-only: severity never triggers an action,
///
/// it only selects the chrome treatment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

impl NotificationSeverity {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Parses a canonical [`Self::as_str`] name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

impl fmt::Display for NotificationSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One queued chrome notice: identity plus bounded body plus severity.
///
/// The runtime assigns no timestamp and no id: the caller supplies the
/// [`NotificationId`] so queue order stays a pure function of call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChromeNotification {
    id: NotificationId,
    body: String,
    severity: NotificationSeverity,
}

impl ChromeNotification {
    /// Builds a notice, rejecting overlong bodies fail-closed.
    pub fn new(
        id: NotificationId,
        body: &str,
        severity: NotificationSeverity,
    ) -> Result<Self, ChromeError> {
        let len = body.chars().count();
        if len > MAX_NOTIFICATION_TEXT_LEN {
            return Err(ChromeError::TextTooLong {
                len,
                cap: MAX_NOTIFICATION_TEXT_LEN,
            });
        }
        Ok(Self {
            id,
            body: body.to_owned(),
            severity,
        })
    }

    /// Returns the notice identity.
    #[must_use]
    pub const fn id(&self) -> NotificationId {
        self.id
    }

    /// Returns the notice body.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Returns the notice severity.
    #[must_use]
    pub const fn severity(&self) -> NotificationSeverity {
        self.severity
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Headless per-window chrome state.
///
/// Defaults: every surface visible, no notices, command surface closed, and
/// an empty overlay root. Visibility is presentation state only: hiding a
/// surface never destroys its queued content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowChromeRuntime {
    visible: [bool; 5],
    notifications: Vec<ChromeNotification>,
    command_target: Option<UiNodeId>,
    overlay_nodes: Vec<UiNodeId>,
}

impl WindowChromeRuntime {
    /// Builds default chrome state: all surfaces visible, nothing queued.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            visible: [true; 5],
            notifications: Vec::new(),
            command_target: None,
            overlay_nodes: Vec::new(),
        }
    }

    /// Sets surface visibility.
    pub fn set_visible(&mut self, surface: ChromeSurface, visible: bool) {
        self.visible[surface.index()] = visible;
    }

    /// Reports surface visibility.
    #[must_use]
    pub const fn is_visible(&self, surface: ChromeSurface) -> bool {
        self.visible[surface.index()]
    }

    /// Queues a notice at the tail, rejecting a full queue or a duplicate
    /// id fail-closed.
    pub fn push_notification(&mut self, notice: ChromeNotification) -> Result<(), ChromeError> {
        if self.notifications.len() >= MAX_NOTIFICATIONS {
            return Err(ChromeError::NotificationsFull {
                cap: MAX_NOTIFICATIONS,
            });
        }
        if self.notifications.iter().any(|n| n.id == notice.id) {
            return Err(ChromeError::DuplicateNotification { id: notice.id });
        }
        self.notifications.push(notice);
        Ok(())
    }

    /// Removes a queued notice by id, rejecting unknown ids fail-closed.
    pub fn dismiss_notification(&mut self, id: NotificationId) -> Result<(), ChromeError> {
        let pos = self.notifications.iter().position(|n| n.id == id);
        match pos {
            Some(index) => {
                self.notifications.remove(index);
                Ok(())
            }
            None => Err(ChromeError::UnknownNotification { id }),
        }
    }

    /// Returns queued notices in insertion order.
    #[must_use]
    pub fn notifications(&self) -> &[ChromeNotification] {
        &self.notifications
    }

    /// Opens the command surface on one chrome node, rejecting a second
    /// open session fail-closed (at most one session per window).
    pub fn open_command(&mut self, target: UiNodeId) -> Result<(), ChromeError> {
        if self.command_target.is_some() {
            return Err(ChromeError::CommandBusy);
        }
        self.command_target = Some(target);
        Ok(())
    }

    /// Closes the command surface session, rejecting a close on a closed
    /// surface fail-closed.
    pub fn close_command(&mut self) -> Result<(), ChromeError> {
        match self.command_target {
            Some(_) => {
                self.command_target = None;
                Ok(())
            }
            None => Err(ChromeError::CommandClosed),
        }
    }

    /// Returns the open command session target, if any.
    #[must_use]
    pub const fn command_target(&self) -> Option<UiNodeId> {
        self.command_target
    }

    /// Attaches a node to the overlay root, rejecting a full root or a
    /// duplicate attachment fail-closed.
    pub fn attach_overlay(&mut self, id: UiNodeId) -> Result<(), ChromeError> {
        if self.overlay_nodes.contains(&id) {
            return Err(ChromeError::DuplicateOverlayNode { id });
        }
        if self.overlay_nodes.len() >= MAX_OVERLAY_NODES {
            return Err(ChromeError::OverlayRootFull {
                cap: MAX_OVERLAY_NODES,
            });
        }
        self.overlay_nodes.push(id);
        Ok(())
    }

    /// Detaches a node from the overlay root, rejecting unknown nodes
    /// fail-closed.
    pub fn detach_overlay(&mut self, id: UiNodeId) -> Result<(), ChromeError> {
        let pos = self.overlay_nodes.iter().position(|n| *n == id);
        match pos {
            Some(index) => {
                self.overlay_nodes.remove(index);
                Ok(())
            }
            None => Err(ChromeError::UnknownOverlayNode { id }),
        }
    }

    /// Returns overlay-root attachments sorted by node id (deterministic).
    #[must_use]
    pub fn overlay_nodes(&self) -> Vec<UiNodeId> {
        let mut nodes = self.overlay_nodes.clone();
        nodes.sort();
        nodes
    }
}

impl Default for WindowChromeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_names_round_trip() {
        for surface in [
            ChromeSurface::WorkspaceRail,
            ChromeSurface::StatusBar,
            ChromeSurface::OverlayRoot,
            ChromeSurface::NotificationArea,
            ChromeSurface::CommandSurface,
        ] {
            assert_eq!(ChromeSurface::parse(surface.as_str()), Some(surface));
        }
        assert_eq!(ChromeSurface::parse("Status-Bar"), None);
        assert_eq!(ChromeSurface::parse(" status-bar"), None);
        assert_eq!(ChromeSurface::parse(""), None);
    }

    #[test]
    fn visibility_defaults_and_toggle() {
        let mut chrome = WindowChromeRuntime::new();
        assert!(chrome.is_visible(ChromeSurface::StatusBar));
        chrome.set_visible(ChromeSurface::StatusBar, false);
        assert!(!chrome.is_visible(ChromeSurface::StatusBar));
        // Toggling one surface leaves the others alone.
        assert!(chrome.is_visible(ChromeSurface::WorkspaceRail));
    }

    #[test]
    fn notification_queue_is_bounded_and_deduped() {
        let mut chrome = WindowChromeRuntime::new();
        for n in 0..MAX_NOTIFICATIONS {
            let id = NotificationId::new(n as u64);
            chrome
                .push_notification(
                    ChromeNotification::new(id, "hello", NotificationSeverity::Info).expect("fits"),
                )
                .expect("queue has room");
        }
        let dup = ChromeNotification::new(
            NotificationId::new(0),
            "again",
            NotificationSeverity::Warning,
        )
        .expect("fits");
        // Full queue reports full before identity is even considered.
        assert_eq!(
            chrome.push_notification(dup),
            Err(ChromeError::NotificationsFull {
                cap: MAX_NOTIFICATIONS
            })
        );
        chrome
            .dismiss_notification(NotificationId::new(0))
            .expect("known id dismisses");
        assert_eq!(
            chrome.push_notification(
                ChromeNotification::new(
                    NotificationId::new(0),
                    "again",
                    NotificationSeverity::Warning
                )
                .expect("fits")
            ),
            Ok(())
        );
        assert_eq!(
            chrome.dismiss_notification(NotificationId::new(999)),
            Err(ChromeError::UnknownNotification {
                id: NotificationId::new(999)
            })
        );
        // Insertion order is preserved for the survivors.
        let ids: Vec<u64> = chrome
            .notifications()
            .iter()
            .map(|n| n.id().get())
            .collect();
        assert_eq!(ids[0], 1);
        assert_eq!(ids[ids.len() - 1], 0);
    }

    #[test]
    fn notification_body_is_bounded() {
        let long = "x".repeat(MAX_NOTIFICATION_TEXT_LEN + 1);
        assert!(matches!(
            ChromeNotification::new(NotificationId::new(1), &long, NotificationSeverity::Error),
            Err(ChromeError::TextTooLong { .. })
        ));
    }

    #[test]
    fn command_surface_holds_one_session() {
        let mut chrome = WindowChromeRuntime::new();
        assert_eq!(chrome.command_target(), None);
        chrome.open_command(UiNodeId::new(7)).expect("opens");
        assert_eq!(chrome.command_target(), Some(UiNodeId::new(7)));
        assert_eq!(
            chrome.open_command(UiNodeId::new(8)),
            Err(ChromeError::CommandBusy)
        );
        chrome.close_command().expect("closes");
        assert_eq!(chrome.close_command(), Err(ChromeError::CommandClosed));
    }

    #[test]
    fn overlay_root_reports_sorted_membership() {
        let mut chrome = WindowChromeRuntime::new();
        chrome.attach_overlay(UiNodeId::new(9)).expect("attaches");
        chrome.attach_overlay(UiNodeId::new(3)).expect("attaches");
        assert_eq!(
            chrome.attach_overlay(UiNodeId::new(3)),
            Err(ChromeError::DuplicateOverlayNode {
                id: UiNodeId::new(3)
            })
        );
        assert_eq!(
            chrome.overlay_nodes(),
            vec![UiNodeId::new(3), UiNodeId::new(9)]
        );
        chrome.detach_overlay(UiNodeId::new(3)).expect("detaches");
        assert_eq!(
            chrome.detach_overlay(UiNodeId::new(3)),
            Err(ChromeError::UnknownOverlayNode {
                id: UiNodeId::new(3)
            })
        );
    }
}
