//! Panel primitives for generic Panel Runtime (CTX-0102, OQ-014 pre-study).
//!
//! This module provides headless, bounded panel types that sit beside `View`
//! and `LayoutNode` without introducing a new tiling primitive. Panels are
//! generic workspace-managed application containers (not OS windows, not PTY)
//! hosted via the compositor as `ViewContent::Panel(PanelId)` (Option A from
//! the pre-study). The runtime owns lifecycle, focus, overlay, and bus
//! mediation; this crate owns presentation types only, with no PTY, GPU, or
//! window handles.
//!
//! All types are bounded and `forbid(unsafe_code)`. No wall-clock or
//! randomness participates; layout and focus remain deterministic.

#![forbid(unsafe_code)]

use crate::geometry::Rect;
use crate::view::ViewId;

// ---------------------------------------------------------------------------
// Identity: PanelId distinct newtype, pairwise incompatible with ViewId / TerminalId
// ---------------------------------------------------------------------------

/// Stable handle for a panel instance. Distinct newtype from `ViewId` and
/// `TerminalId`; no `From` bridge exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PanelId(pub u64);

impl PanelId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PanelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PanelId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Identity: BrowserSurfaceId distinct newtype, pairwise incompatible with PanelId / ViewId / TerminalId
// ---------------------------------------------------------------------------

/// Stable handle for a browser WebView surface. Distinct newtype from
/// `PanelId`, `ViewId`, `TerminalId`; no `From` bridge exists.
/// Lifecycle parallels `PanelId` with `Generation` monotonic reserve `1024`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BrowserSurfaceId(pub u64);

impl BrowserSurfaceId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for BrowserSurfaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BrowserSurfaceId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Lifecycle: Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed
// ---------------------------------------------------------------------------

/// Panel lifecycle states (CTX-0102).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelState {
    /// Manifest-declared but not yet allocated.
    Declared,
    /// Allocated with `(PanelId, Generation)` but not mounted.
    Created,
    /// Bound to an empty `ViewId` via `PanelRuntime::mount`.
    Mounted,
    /// Focused within its `Workspace`; owns keyboard/IME/wheel routing.
    Focused,
    /// Invisible (inactive workspace, scratchpad hidden, zero-area, overlay
    /// occluded) without destroying attachment.
    Suspended,
    /// All resources released; `(PanelId, Generation)` retired.
    Disposed,
}

impl std::fmt::Display for PanelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Declared => "Declared",
            Self::Created => "Created",
            Self::Mounted => "Mounted",
            Self::Focused => "Focused",
            Self::Suspended => "Suspended",
            Self::Disposed => "Disposed",
        };
        f.write_str(s)
    }
}

impl PanelState {
    /// Whether transition `from -> to` is allowed.
    #[must_use]
    pub fn can_transition(from: Self, to: Self) -> bool {
        matches!(
            (from, to),
            (Self::Declared, Self::Created)
                | (Self::Created, Self::Mounted)
                | (Self::Mounted, Self::Focused)
                | (Self::Mounted, Self::Suspended)
                | (Self::Focused, Self::Suspended)
                | (Self::Focused, Self::Mounted)
                | (Self::Suspended, Self::Mounted)
                | (Self::Suspended, Self::Focused)
                | (Self::Created, Self::Disposed)
                | (Self::Mounted, Self::Disposed)
                | (Self::Focused, Self::Disposed)
                | (Self::Suspended, Self::Disposed)
        )
    }
}

// ---------------------------------------------------------------------------
// PanelType closed v1 candidate set
// ---------------------------------------------------------------------------

/// Closed set of panel types contributed by a `PanelProvider`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PanelType {
    Terminal,
    Rich,
    Browser,
    Helper,
    Canvas,
}

impl PanelType {
    /// Parse a panel type string (lowercase, closed set).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "terminal" => Some(Self::Terminal),
            "rich" => Some(Self::Rich),
            "browser" => Some(Self::Browser),
            "helper" => Some(Self::Helper),
            "canvas" => Some(Self::Canvas),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Rich => "rich",
            Self::Browser => "browser",
            Self::Helper => "helper",
            Self::Canvas => "canvas",
        }
    }
}

impl std::fmt::Display for PanelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// ViewContent: typed View content (Option A preference)
// ---------------------------------------------------------------------------

/// Content hosted inside a `View` leaf. Generic Panel Runtime adds
/// `Panel(PanelId)` as a fifth variant without adding a new tiling primitive.
/// `LayoutNode` and decoration stay Core-owned. Browser is host-owned
/// `BrowserSurfaceId` via embedder (Option A of browser-agent pre-study).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ViewContent {
    Empty,
    Terminal(u64),
    Rich(u64),
    Browser(BrowserSurfaceId),
    Panel(PanelId),
}

impl ViewContent {
    #[must_use]
    pub fn is_panel(self) -> bool {
        matches!(self, Self::Panel(_))
    }

    #[must_use]
    pub fn panel_id(self) -> Option<PanelId> {
        match self {
            Self::Panel(id) => Some(id),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_browser(self) -> bool {
        matches!(self, Self::Browser(_))
    }

    #[must_use]
    pub fn browser_id(self) -> Option<BrowserSurfaceId> {
        match self {
            Self::Browser(id) => Some(id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay: ephemeral presentation surface per window, bounded 4+1
// ---------------------------------------------------------------------------

pub const MAX_OVERLAYS_PER_WINDOW: usize = 4;
pub const MAX_OVERLAY_TEXT_LEN: usize = 128;
pub const MAX_OVERLAY_TOOLTIP_LEN: usize = 256;

/// Overlay kind (presentation-only, never mutates grid).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayKind {
    /// At most one modal per window; second request returns `OverlayBusy`.
    Modal,
    NonModal,
    Tooltip,
    Palette,
}

/// Overlay surface owned by the compositor, not a tiling leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlay {
    pub id: u64,
    pub kind: OverlayKind,
    pub bounds: Rect,
    pub text: String,
    pub tooltip: Option<String>,
    pub generation: u64,
    pub truncated: bool,
}

impl Overlay {
    /// Creates an overlay with bounded text/tooltip. Truncates at char
    /// boundary and sets `truncated` flag when over limit.
    pub fn new(
        id: u64,
        kind: OverlayKind,
        bounds: Rect,
        text: impl Into<String>,
        tooltip: Option<String>,
        generation: u64,
    ) -> Self {
        let raw_text = text.into();
        let (text, truncated) = truncate_bounded(&raw_text, MAX_OVERLAY_TEXT_LEN);
        let (tooltip, tooltip_truncated) = match tooltip {
            Some(t) => {
                let (s, trunc) = truncate_bounded(&t, MAX_OVERLAY_TOOLTIP_LEN);
                (Some(s), trunc)
            }
            None => (None, false),
        };
        Self {
            id,
            kind,
            bounds,
            text,
            tooltip,
            generation,
            truncated: truncated || tooltip_truncated,
        }
    }

    #[must_use]
    pub fn is_modal(&self) -> bool {
        self.kind == OverlayKind::Modal
    }
}

fn truncate_bounded(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let truncated: String = s.chars().take(max_chars).collect();
    (truncated, true)
}

/// Per-window overlay manager enforcing `4+1` bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayManager {
    overlays: Vec<Overlay>,
    next_id: u64,
}

impl OverlayManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            overlays: Vec::new(),
            next_id: 1,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    #[must_use]
    pub fn modal_active(&self) -> bool {
        self.overlays.iter().any(|o| o.is_modal())
    }

    /// Creates an overlay; enforces modal exclusivity and `4+1` bound.
    ///
    /// # Errors
    /// `OverlayBusy` when modal already active and new is modal,
    /// `TooManyOverlays` when non-modal limit reached.
    pub fn create_overlay(
        &mut self,
        kind: OverlayKind,
        bounds: Rect,
        text: impl Into<String>,
        tooltip: Option<String>,
        generation: u64,
    ) -> Result<u64, OverlayError> {
        if kind == OverlayKind::Modal && self.modal_active() {
            return Err(OverlayError::OverlayBusy);
        }
        let non_modal_count = self.overlays.iter().filter(|o| !o.is_modal()).count();
        let modal_count = self.overlays.iter().filter(|o| o.is_modal()).count();
        if kind == OverlayKind::Modal {
            if modal_count >= 1 {
                return Err(OverlayError::OverlayBusy);
            }
        } else if non_modal_count >= MAX_OVERLAYS_PER_WINDOW {
            return Err(OverlayError::TooManyOverlays {
                max: MAX_OVERLAYS_PER_WINDOW,
                current: non_modal_count,
            });
        }
        // Global bound check: total <= 5
        if self.overlays.len() > MAX_OVERLAYS_PER_WINDOW {
            return Err(OverlayError::TooManyOverlays {
                max: MAX_OVERLAYS_PER_WINDOW,
                current: self.overlays.len(),
            });
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let overlay = Overlay::new(id, kind, bounds, text, tooltip, generation);
        self.overlays.push(overlay);
        Ok(id)
    }

    pub fn dismiss(&mut self, id: u64) -> Option<Overlay> {
        if let Some(pos) = self.overlays.iter().position(|o| o.id == id) {
            Some(self.overlays.remove(pos))
        } else {
            None
        }
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Overlay> {
        self.overlays.iter().find(|o| o.id == id)
    }

    pub fn overlays(&self) -> &[Overlay] {
        &self.overlays
    }

    pub fn clear(&mut self) {
        self.overlays.clear();
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayError {
    OverlayBusy,
    TooManyOverlays { max: usize, current: usize },
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverlayBusy => f.write_str("modal overlay already active (OverlayBusy)"),
            Self::TooManyOverlays { max, current } => {
                write!(f, "too many overlays: max {max}, current {current}")
            }
        }
    }
}

impl std::error::Error for OverlayError {}

// ---------------------------------------------------------------------------
// Command registry: owner.name:command, duplicates rejected
// ---------------------------------------------------------------------------

pub const MAX_COMMANDS_PER_PANEL_TYPE: usize = 32;
pub const MAX_COMMAND_LEN: usize = 128;

/// Qualified command name `owner.name:command`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QualifiedCommand(String);

impl QualifiedCommand {
    /// Validate `owner.name:command` with `^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z][a-z0-9_.-]*$`, `<=64`?
    /// For commands we allow `<=128` chars but same grammar.
    pub fn parse(raw: &str) -> Result<Self, CommandError> {
        if raw.is_empty() {
            return Err(CommandError::Invalid(
                "command must not be empty".to_string(),
            ));
        }
        if raw.len() > MAX_COMMAND_LEN {
            return Err(CommandError::Invalid(format!(
                "command exceeds {MAX_COMMAND_LEN} bytes"
            )));
        }
        let (owner_part, command) = raw.split_once(':').ok_or_else(|| {
            CommandError::Invalid("command must contain ':' as owner.name:command".to_string())
        })?;
        if command.is_empty() {
            return Err(CommandError::Invalid(
                "command name must not be empty".to_string(),
            ));
        }
        if command.len() > 64 {
            return Err(CommandError::Invalid(
                "command name exceeds 64 bytes".to_string(),
            ));
        }
        if !command
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            return Err(CommandError::Invalid(
                "command name must be [a-zA-Z0-9_.-]".to_string(),
            ));
        }
        if !command
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        {
            return Err(CommandError::Invalid(
                "command name must start with alphabetic".to_string(),
            ));
        }
        // owner_part must be `owner.name` with two segments
        let parts: Vec<&str> = owner_part.split('.').collect();
        if parts.len() != 2 {
            return Err(CommandError::Invalid(
                "owner must be owner.name (two dot segments)".to_string(),
            ));
        }
        for seg in &parts {
            if seg.is_empty() {
                return Err(CommandError::Invalid("owner segment empty".to_string()));
            }
            if seg.len() > 32 {
                return Err(CommandError::Invalid(
                    "owner segment exceeds 32 bytes".to_string(),
                ));
            }
            let first = seg.as_bytes()[0];
            if !first.is_ascii_lowercase() {
                return Err(CommandError::Invalid(
                    "owner segment must start with lowercase letter".to_string(),
                ));
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(CommandError::Invalid(
                    "owner segment must be [a-z0-9_-]".to_string(),
                ));
            }
        }
        // Validate no control/whitespace
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(CommandError::Invalid(
                "command must not contain control or whitespace".to_string(),
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for QualifiedCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    Invalid(String),
    Duplicate { command: String, owner: PanelId },
    TooManyCommands { max: usize, current: usize },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid command: {msg}"),
            Self::Duplicate { command, owner } => {
                write!(f, "duplicate command '{command}' already owned by {owner}")
            }
            Self::TooManyCommands { max, current } => {
                write!(f, "too many commands: max {max}, current {current}")
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// Command registry per window, Core-owned, generation-aware.
#[derive(Clone, Debug, Default)]
pub struct CommandRegistry {
    commands: std::collections::HashMap<String, PanelId>,
    per_panel: std::collections::HashMap<PanelId, Vec<String>>,
}

impl CommandRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: std::collections::HashMap::new(),
            per_panel: std::collections::HashMap::new(),
        }
    }

    /// Register a command for `panel_id`. Validates grammar and enforces
    /// `<=32` per panel type and duplicate rejection.
    pub fn register(
        &mut self,
        panel_id: PanelId,
        raw: &str,
    ) -> Result<QualifiedCommand, CommandError> {
        let qc = QualifiedCommand::parse(raw)?;
        if let Some(owner) = self.commands.get(qc.as_str()) {
            return Err(CommandError::Duplicate {
                command: qc.as_str().to_string(),
                owner: *owner,
            });
        }
        let count = self.per_panel.get(&panel_id).map_or(0, |v| v.len());
        if count >= MAX_COMMANDS_PER_PANEL_TYPE {
            return Err(CommandError::TooManyCommands {
                max: MAX_COMMANDS_PER_PANEL_TYPE,
                current: count,
            });
        }
        self.commands.insert(qc.as_str().to_string(), panel_id);
        self.per_panel
            .entry(panel_id)
            .or_default()
            .push(qc.as_str().to_string());
        Ok(qc)
    }

    #[must_use]
    pub fn owner_of(&self, command: &str) -> Option<PanelId> {
        self.commands.get(command).copied()
    }

    #[must_use]
    pub fn commands_for(&self, panel_id: PanelId) -> Vec<String> {
        self.per_panel.get(&panel_id).cloned().unwrap_or_default()
    }

    pub fn unregister_panel(&mut self, panel_id: PanelId) {
        if let Some(cmds) = self.per_panel.remove(&panel_id) {
            for c in cmds {
                self.commands.remove(&c);
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers for panel presentation (reuse Rect)
// ---------------------------------------------------------------------------

/// Re-exported for panel overlay bounds validation.
pub fn validate_panel_bounds(bounds: Rect, container: Rect) -> Option<Rect> {
    bounds.clip_to(container)
}

// ---------------------------------------------------------------------------
// Focus MRU for panels per Workspace
// ---------------------------------------------------------------------------

/// Panel focus state per workspace with MRU ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelFocus {
    focused: Option<PanelId>,
    mru: std::collections::VecDeque<PanelId>,
}

impl PanelFocus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            focused: None,
            mru: std::collections::VecDeque::new(),
        }
    }

    #[must_use]
    pub fn focused(&self) -> Option<PanelId> {
        self.focused
    }

    pub fn set(&mut self, id: PanelId) {
        self.focused = Some(id);
        self.mru.retain(|&x| x != id);
        self.mru.push_front(id);
    }

    pub fn clear(&mut self) {
        self.focused = None;
    }

    #[must_use]
    pub fn mru_order(&self) -> Vec<PanelId> {
        self.mru.iter().copied().collect()
    }

    /// On hide/detach of focused panel, move to next MRU.
    pub fn on_panel_hidden(&mut self, hidden: PanelId) {
        self.mru.retain(|&x| x != hidden);
        if self.focused == Some(hidden) {
            if let Some(&next) = self.mru.front() {
                self.focused = Some(next);
            } else {
                self.focused = None;
            }
        }
    }

    /// Returns true when `id` is focused.
    #[must_use]
    pub fn is_focused(&self, id: PanelId) -> bool {
        self.focused == Some(id)
    }
}

impl Default for PanelFocus {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PanelFocusRouter: keyboard/IME/wheel routing to focused panel
// ---------------------------------------------------------------------------

/// Routing target for input events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputTarget {
    Panel(PanelId),
    View(ViewId),
    None,
}

/// Determines input target given panel focus vs view focus.
/// When a panel is focused, it owns routing; otherwise view focus applies.
/// Mirrors the spec routing: `Platform -> Router -> focused Panel/View -> keymap`.
#[must_use]
pub fn route_input(panel_focus: Option<PanelId>, view_focus: Option<ViewId>) -> InputTarget {
    if let Some(pid) = panel_focus {
        InputTarget::Panel(pid)
    } else if let Some(vid) = view_focus {
        InputTarget::View(vid)
    } else {
        InputTarget::None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn panel_id_distinct_from_view_id() {
        let pid = PanelId::new(1);
        let vid = ViewId::new(1);
        assert_eq!(pid.0, vid.0);
        assert_ne!(
            std::any::TypeId::of::<PanelId>(),
            std::any::TypeId::of::<ViewId>()
        );
        // No From bridge — compile-time guarantee; runtime check that
        // TypeIds differ proves distinctness.
    }

    #[test]
    fn lifecycle_transitions() {
        assert!(PanelState::can_transition(
            PanelState::Declared,
            PanelState::Created
        ));
        assert!(PanelState::can_transition(
            PanelState::Created,
            PanelState::Mounted
        ));
        assert!(!PanelState::can_transition(
            PanelState::Declared,
            PanelState::Focused
        ));
        assert!(PanelState::can_transition(
            PanelState::Focused,
            PanelState::Suspended
        ));
        assert!(PanelState::can_transition(
            PanelState::Suspended,
            PanelState::Mounted
        ));
        assert!(PanelState::can_transition(
            PanelState::Mounted,
            PanelState::Disposed
        ));
    }

    #[test]
    fn panel_type_closed() {
        assert_eq!(PanelType::parse("terminal"), Some(PanelType::Terminal));
        assert_eq!(PanelType::parse("unknown"), None);
        assert_eq!(PanelType::parse("Terminal"), None);
    }

    #[test]
    fn overlay_manager_enforces_4plus1() {
        let mut mgr = OverlayManager::new();
        let container = Rect::new(0, 0, 80, 24);
        // 4 non-modal ok
        for _ in 0..4 {
            mgr.create_overlay(OverlayKind::NonModal, container, "hello", None, 1)
                .unwrap();
        }
        assert_eq!(mgr.len(), 4);
        // 5th non-modal fails
        let err = mgr
            .create_overlay(OverlayKind::NonModal, container, "hello", None, 1)
            .unwrap_err();
        assert!(matches!(err, OverlayError::TooManyOverlays { .. }));
        // modal still allowed (4+1)
        mgr.create_overlay(OverlayKind::Modal, container, "modal", None, 1)
            .unwrap();
        assert_eq!(mgr.len(), 5);
        // second modal fails
        let err2 = mgr
            .create_overlay(OverlayKind::Modal, container, "modal2", None, 1)
            .unwrap_err();
        assert_eq!(err2, OverlayError::OverlayBusy);
        // dismiss one non-modal, can add another
        let first_id = mgr.overlays()[0].id;
        mgr.dismiss(first_id);
        assert_eq!(mgr.len(), 4);
        mgr.create_overlay(OverlayKind::NonModal, container, "again", None, 1)
            .unwrap();
        assert_eq!(mgr.len(), 5);
    }

    #[test]
    fn overlay_text_truncation() {
        let long = "a".repeat(200);
        let o = Overlay::new(
            1,
            OverlayKind::NonModal,
            Rect::new(0, 0, 10, 10),
            long,
            None,
            1,
        );
        assert_eq!(o.text.chars().count(), MAX_OVERLAY_TEXT_LEN);
        assert!(o.truncated);
        let short = "hi";
        let o2 = Overlay::new(
            2,
            OverlayKind::NonModal,
            Rect::new(0, 0, 10, 10),
            short,
            None,
            1,
        );
        assert!(!o2.truncated);
    }

    #[test]
    fn command_registry_owner_name_command() {
        let mut reg = CommandRegistry::new();
        let pid = PanelId::new(1);
        let qc = reg.register(pid, "xuepoo.git:open").unwrap();
        assert_eq!(qc.as_str(), "xuepoo.git:open");
        // Duplicate across panels rejected
        let pid2 = PanelId::new(2);
        let err = reg.register(pid2, "xuepoo.git:open").unwrap_err();
        assert!(matches!(err, CommandError::Duplicate { .. }));
        // Invalid grammar
        assert!(reg.register(pid, "badcommand").is_err());
        assert!(reg.register(pid, "OWNER.name:cmd").is_err());
        assert!(reg.register(pid, "xuepoo.git:").is_err());
        // Per-panel limit
        let mut reg2 = CommandRegistry::new();
        let pid3 = PanelId::new(3);
        for i in 0..MAX_COMMANDS_PER_PANEL_TYPE {
            reg2.register(pid3, &format!("xuepoo.test:cmd{i}")).unwrap();
        }
        let err2 = reg2.register(pid3, "xuepoo.test:overflow").unwrap_err();
        assert!(matches!(err2, CommandError::TooManyCommands { .. }));
    }

    #[test]
    fn panel_focus_mru() {
        let mut focus = PanelFocus::new();
        let p1 = PanelId::new(1);
        let p2 = PanelId::new(2);
        let p3 = PanelId::new(3);
        focus.set(p1);
        focus.set(p2);
        focus.set(p3);
        assert_eq!(focus.focused(), Some(p3));
        assert_eq!(focus.mru_order(), vec![p3, p2, p1]);
        // Hidden focused moves to next MRU
        focus.on_panel_hidden(p3);
        assert_eq!(focus.focused(), Some(p2));
        assert_eq!(focus.mru_order(), vec![p2, p1]);
        // Hidden non-focused just removes from MRU
        focus.set(p3);
        focus.on_panel_hidden(p1);
        assert_eq!(focus.mru_order(), vec![p3, p2]);
    }

    #[test]
    fn input_routing() {
        let pid = PanelId::new(42);
        let vid = ViewId::new(7);
        assert_eq!(route_input(Some(pid), Some(vid)), InputTarget::Panel(pid));
        assert_eq!(route_input(None, Some(vid)), InputTarget::View(vid));
        assert_eq!(route_input(None, None), InputTarget::None);
    }

    #[test]
    fn view_content_panel() {
        let pid = PanelId::new(10);
        let vc = ViewContent::Panel(pid);
        assert!(vc.is_panel());
        assert_eq!(vc.panel_id(), Some(pid));
        let empty = ViewContent::Empty;
        assert!(!empty.is_panel());
    }

    #[test]
    fn view_content_browser() {
        let bid = BrowserSurfaceId::new(10);
        let vc = ViewContent::Browser(bid);
        assert!(vc.is_browser());
        assert_eq!(vc.browser_id(), Some(bid));
        assert!(!vc.is_panel());
        let empty = ViewContent::Empty;
        assert!(!empty.is_browser());
    }

    #[test]
    fn browser_surface_id_distinct_from_panel_and_view() {
        let bid = BrowserSurfaceId::new(1);
        let pid = PanelId::new(1);
        let vid = ViewId::new(1);
        assert_eq!(bid.0, pid.0);
        assert_eq!(bid.0, vid.0);
        assert_ne!(
            std::any::TypeId::of::<BrowserSurfaceId>(),
            std::any::TypeId::of::<PanelId>()
        );
        assert_ne!(
            std::any::TypeId::of::<BrowserSurfaceId>(),
            std::any::TypeId::of::<ViewId>()
        );
    }

    #[test]
    fn validate_panel_bounds_clipping() {
        let container = Rect::new(0, 0, 100, 100);
        let bounds = Rect::new(10, 10, 20, 20);
        assert_eq!(
            crate::panel::validate_panel_bounds(bounds, container),
            Some(bounds)
        );
        let outside = Rect::new(200, 200, 10, 10);
        assert_eq!(
            crate::panel::validate_panel_bounds(outside, container),
            None
        );
    }
}
