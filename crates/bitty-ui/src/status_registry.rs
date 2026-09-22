#![forbid(unsafe_code)]
//! Status System v1 contracts (CW-24, issue #1002; `status-system.md` draft).
//!
//! Contract source: the draft Status System Specification, which imports the
//! Waybar module philosophy into Bitty as typed, validated registry entries:
//!
//! - the v1 module set (`workspace`, `cwd`, `git`, `cpu`, `memory`,
//!   `network`, `battery`, `clock`) plus the `Provider:status.component`
//!   extension point;
//! - the Status Module Registry binding identifiers to ordered
//!   `left`/`center`/`right` slots, mirroring Waybar
//!   `modules-left`/`modules-center`/`modules-right`;
//! - the Platform Core-owned `SystemMetricsService` as the sole sampler for
//!   `cpu`/`memory`/`network` (see `bitty-platform` `metrics` module);
//! - Provider `status.component` composition as declarative values the
//!   registry composes without handing out a mutable bar handle.
//!
//! What this module provides:
//!
//! - [`StatusModuleId`] — the closed v1 identifier set plus qualified
//!   `owner.name:component` provider identifiers; unknown bare identifiers
//!   fail validation.
//! - [`StatusSlots`] — ordered slot assignment with unique-membership
//!   validation (listing one identifier in two slots is an error; empty
//!   slots are valid and collapse).
//! - [`ModuleSegment`] — bounded presentation segment (`text <= 64` chars,
//!   `icon <= 16`, `tooltip <= 128`); over-length input truncates at a char
//!   boundary with an explicit flag, never a panic.
//! - [`StatusInputs`] + [`render_module`] — pure headless composition over a
//!   bounded snapshot. Missing inputs render as `—` (except `battery`,
//!   which hides when no battery exists); Terminal Truth is never read or
//!   mutated here.
//!
//! No status module runs on a hot path; sampling and composition occur on
//! the presentation cold path with explicit cadence caps owned by the
//! metrics service.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum total modules (built-ins plus provider components) on one bar.
pub const STATUS_MAX_MODULES: usize = 16;

/// Maximum rendered segment text length, in characters.
pub const STATUS_TEXT_MAX_CHARS: usize = 64;

/// Maximum icon length, in characters.
pub const STATUS_ICON_MAX_CHARS: usize = 16;

/// Maximum tooltip length, in characters.
pub const STATUS_TOOLTIP_MAX_CHARS: usize = 128;

/// Maximum provider component name length, in bytes.
pub const STATUS_COMPONENT_MAX_LEN: usize = 32;

/// Maximum owner segment (`owner`, `name`) length, in bytes.
pub const STATUS_OWNER_SEGMENT_MAX_LEN: usize = 16;

/// Rendered placeholder for a missing or failed module value.
pub const STATUS_MISSING_TEXT: &str = "—";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed registry failure; composition keeps its previous state on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusRegistryError {
    /// Bare identifier outside the closed v1 set.
    UnknownModule { value: String },
    /// Qualified provider identifier with invalid shape.
    InvalidQualifiedName { value: String, reason: String },
    /// One identifier listed in more than one slot.
    DuplicateMembership { value: String },
    /// Bar segment cap exceeded.
    TooManyModules { max: usize, current: usize },
}

impl std::fmt::Display for StatusRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownModule { value } => write!(f, "unknown status module '{value}'"),
            Self::InvalidQualifiedName { value, reason } => {
                write!(f, "invalid provider module '{value}': {reason}")
            }
            Self::DuplicateMembership { value } => {
                write!(f, "status module '{value}' listed in more than one slot")
            }
            Self::TooManyModules { max, current } => {
                write!(f, "too many status modules: max {max}, current {current}")
            }
        }
    }
}

impl std::error::Error for StatusRegistryError {}

// ---------------------------------------------------------------------------
// Module identity
// ---------------------------------------------------------------------------

/// Provider-contributed module identity: `owner.name:component`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProviderComponent {
    owner: String,
    name: String,
}

impl ProviderComponent {
    /// Qualified registry key (`owner.name:component`).
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}:{}", self.owner, self.name)
    }

    /// Provider id (`owner.name`).
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Component name within the provider.
    #[must_use]
    pub fn component(&self) -> &str {
        &self.name
    }
}

/// v1 status module identity: eight built-ins plus provider components.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatusModuleId {
    Workspace,
    Cwd,
    Git,
    Cpu,
    Memory,
    Network,
    Battery,
    Clock,
    Provider(ProviderComponent),
}

impl StatusModuleId {
    /// Parses a registry identifier: a closed-set bare name or a qualified
    /// `owner.name:component` provider name.
    ///
    /// # Errors
    ///
    /// [`StatusRegistryError::UnknownModule`] for unknown bare names, or
    /// [`StatusRegistryError::InvalidQualifiedName`] for malformed
    /// qualified names.
    pub fn parse(raw: &str) -> Result<Self, StatusRegistryError> {
        match raw {
            "workspace" => Ok(Self::Workspace),
            "cwd" => Ok(Self::Cwd),
            "git" => Ok(Self::Git),
            "cpu" => Ok(Self::Cpu),
            "memory" => Ok(Self::Memory),
            "network" => Ok(Self::Network),
            "battery" => Ok(Self::Battery),
            "clock" => Ok(Self::Clock),
            _ => Self::parse_provider(raw),
        }
    }

    fn parse_provider(raw: &str) -> Result<Self, StatusRegistryError> {
        let reject = |reason: &str| StatusRegistryError::InvalidQualifiedName {
            value: raw.to_string(),
            reason: reason.to_string(),
        };
        if raw.is_empty() {
            return Err(reject("identifier must not be empty"));
        }
        if raw.len() > STATUS_TEXT_MAX_CHARS {
            return Err(reject("identifier exceeds 64 bytes"));
        }
        if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(reject("identifier must not contain whitespace"));
        }
        if !raw.contains(':') {
            return Err(StatusRegistryError::UnknownModule {
                value: raw.to_string(),
            });
        }
        let (owner_part, component) = raw
            .split_once(':')
            .ok_or_else(|| reject("identifier must be owner.name:component"))?;
        if component.is_empty() || component.len() > STATUS_COMPONENT_MAX_LEN {
            return Err(reject("component must be 1..=32 bytes"));
        }
        if !is_valid_component(component) {
            return Err(reject("component must start lowercase and use [a-z0-9_.-]"));
        }
        let segments: Vec<&str> = owner_part.split('.').collect();
        if segments.len() != 2 {
            return Err(reject("owner must be owner.name"));
        }
        for segment in &segments {
            if !is_valid_owner_segment(segment) {
                return Err(reject(
                    "owner segments must start lowercase, use [a-z0-9_-], 1..=16 bytes",
                ));
            }
        }
        Ok(Self::Provider(ProviderComponent {
            owner: owner_part.to_string(),
            name: component.to_string(),
        }))
    }

    /// Registry key for this identifier.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Workspace => "workspace".to_string(),
            Self::Cwd => "cwd".to_string(),
            Self::Git => "git".to_string(),
            Self::Cpu => "cpu".to_string(),
            Self::Memory => "memory".to_string(),
            Self::Network => "network".to_string(),
            Self::Battery => "battery".to_string(),
            Self::Clock => "clock".to_string(),
            Self::Provider(component) => component.qualified(),
        }
    }
}

impl std::fmt::Display for StatusModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

fn is_valid_owner_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > STATUS_OWNER_SEGMENT_MAX_LEN {
        return false;
    }
    let mut chars = segment.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    segment
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn is_valid_component(component: &str) -> bool {
    let mut chars = component.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    component
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.')
}

// ---------------------------------------------------------------------------
// Slot registry
// ---------------------------------------------------------------------------

/// Ordered bar slot assignment, mirroring Waybar left/center/right.
/// Render order within a slot equals declaration order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSlots {
    /// Left slot identifiers, in render order.
    pub left: Vec<StatusModuleId>,
    /// Center slot identifiers, in render order.
    pub center: Vec<StatusModuleId>,
    /// Right slot identifiers, in render order.
    pub right: Vec<StatusModuleId>,
}

impl StatusSlots {
    /// Validates unique membership and the total segment cap.
    /// Empty slots are valid and collapse.
    ///
    /// # Errors
    ///
    /// [`StatusRegistryError::DuplicateMembership`] or
    /// [`StatusRegistryError::TooManyModules`].
    pub fn validate(&self) -> Result<(), StatusRegistryError> {
        let total = self.left.len() + self.center.len() + self.right.len();
        if total > STATUS_MAX_MODULES {
            return Err(StatusRegistryError::TooManyModules {
                max: STATUS_MAX_MODULES,
                current: total,
            });
        }
        let mut seen = std::collections::HashSet::new();
        for id in self
            .left
            .iter()
            .chain(self.center.iter())
            .chain(self.right.iter())
        {
            if !seen.insert(id.key()) {
                return Err(StatusRegistryError::DuplicateMembership { value: id.key() });
            }
        }
        Ok(())
    }

    /// Composition order: `left`, then `center`, then `right`.
    #[must_use]
    pub fn render_order(&self) -> Vec<StatusModuleId> {
        self.left
            .iter()
            .chain(self.center.iter())
            .chain(self.right.iter())
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Segments and headless composition
// ---------------------------------------------------------------------------

/// Truncates to `max_chars` characters at a char boundary.
/// Returns the (possibly unchanged) string and whether truncation occurred.
fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_string(), false);
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map_or(s.len(), |(idx, _)| idx);
    (s[..end].to_string(), true)
}

/// Bounded presentation segment contributed by one module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleSegment {
    /// Source module.
    pub id: StatusModuleId,
    /// Rendered text, bounded to [`STATUS_TEXT_MAX_CHARS`] characters.
    pub text: String,
    /// Optional icon, bounded to [`STATUS_ICON_MAX_CHARS`] characters.
    pub icon: Option<String>,
    /// Optional tooltip, bounded to [`STATUS_TOOLTIP_MAX_CHARS`] characters.
    pub tooltip: Option<String>,
    /// Overflow tie-breaker; `clock` and `workspace` default highest.
    pub priority: u8,
    /// Whether any field was truncated to fit its bound.
    pub truncated: bool,
}

impl ModuleSegment {
    /// Builds a segment, truncating over-length fields at char boundaries.
    /// Construction is infallible by design: truncation is explicit via
    /// [`ModuleSegment::truncated`] and never panics.
    #[must_use]
    pub fn new(
        id: StatusModuleId,
        text: &str,
        icon: Option<&str>,
        tooltip: Option<&str>,
        priority: u8,
    ) -> Self {
        let (text, text_truncated) = truncate_chars(text, STATUS_TEXT_MAX_CHARS);
        let (icon, icon_truncated) = icon.map_or((None, false), |icon| {
            let (value, truncated) = truncate_chars(icon, STATUS_ICON_MAX_CHARS);
            (Some(value), truncated)
        });
        let (tooltip, tooltip_truncated) = tooltip.map_or((None, false), |tooltip| {
            let (value, truncated) = truncate_chars(tooltip, STATUS_TOOLTIP_MAX_CHARS);
            (Some(value), truncated)
        });
        Self {
            id,
            text,
            icon,
            tooltip,
            priority,
            truncated: text_truncated || icon_truncated || tooltip_truncated,
        }
    }
}

/// Bounded read-only snapshot a module may render from.
/// Modules read only their declared fields; there is no ambient OS or IO
/// access inside composition. Metric fields are filled by the Platform
/// Core-owned metrics service, never by direct file reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusInputs {
    /// Window/workspace identity label.
    pub workspace: Option<String>,
    /// Active terminal cwd (via OSC 7 / shell integration).
    pub cwd: Option<String>,
    /// Git branch for the cwd snapshot.
    pub git_branch: Option<String>,
    /// Git dirty state for the cwd snapshot.
    pub git_dirty: Option<bool>,
    /// CPU usage percent from the metrics service.
    pub cpu_percent: Option<f32>,
    /// Memory usage percent from the metrics service.
    pub memory_percent: Option<f32>,
    /// Network summary from the metrics service.
    pub network_summary: Option<String>,
    /// Battery percent; `None` hides the module.
    pub battery_percent: Option<u8>,
    /// Pre-formatted clock text (locale-aware formatting happens upstream).
    pub clock_text: String,
}

fn percent_text(percent: Option<f32>) -> String {
    percent.map_or_else(
        || STATUS_MISSING_TEXT.to_string(),
        |value| format!("{}%", value.round() as i64),
    )
}

/// Renders one module from the snapshot. Returns `None` only for `battery`
/// without a battery (hidden); every other missing input renders as `—`.
#[must_use]
pub fn render_module(id: &StatusModuleId, inputs: &StatusInputs) -> Option<ModuleSegment> {
    let segment = match id {
        StatusModuleId::Workspace => ModuleSegment::new(
            id.clone(),
            inputs.workspace.as_deref().unwrap_or(STATUS_MISSING_TEXT),
            None,
            None,
            100,
        ),
        StatusModuleId::Cwd => {
            let (text, _) = truncate_chars(
                inputs.cwd.as_deref().unwrap_or(STATUS_MISSING_TEXT),
                STATUS_TEXT_MAX_CHARS,
            );
            ModuleSegment::new(id.clone(), &text, None, inputs.cwd.as_deref(), 50)
        }
        StatusModuleId::Git => {
            let mut text = inputs
                .git_branch
                .clone()
                .unwrap_or_else(|| STATUS_MISSING_TEXT.to_string());
            if inputs.git_dirty == Some(true) {
                let _ = write!(text, "*");
            }
            ModuleSegment::new(id.clone(), &text, None, None, 50)
        }
        StatusModuleId::Cpu => ModuleSegment::new(
            id.clone(),
            &format!("cpu {}", percent_text(inputs.cpu_percent)),
            None,
            None,
            10,
        ),
        StatusModuleId::Memory => ModuleSegment::new(
            id.clone(),
            &format!("mem {}", percent_text(inputs.memory_percent)),
            None,
            None,
            10,
        ),
        StatusModuleId::Network => ModuleSegment::new(
            id.clone(),
            inputs
                .network_summary
                .as_deref()
                .unwrap_or(STATUS_MISSING_TEXT),
            None,
            None,
            10,
        ),
        StatusModuleId::Battery => {
            let percent = inputs.battery_percent?;
            ModuleSegment::new(id.clone(), &format!("bat {percent}%"), None, None, 10)
        }
        StatusModuleId::Clock => {
            let text = if inputs.clock_text.is_empty() {
                STATUS_MISSING_TEXT.to_string()
            } else {
                inputs.clock_text.clone()
            };
            ModuleSegment::new(id.clone(), &text, None, None, 100)
        }
        StatusModuleId::Provider(component) => ModuleSegment::new(
            id.clone(),
            STATUS_MISSING_TEXT,
            None,
            Some(component.qualified()).as_deref(),
            1,
        ),
    };
    Some(segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots_of(left: &[&str], center: &[&str], right: &[&str]) -> StatusSlots {
        let parse_all = |ids: &[&str]| {
            ids.iter()
                .map(|raw| StatusModuleId::parse(raw).unwrap())
                .collect()
        };
        StatusSlots {
            left: parse_all(left),
            center: parse_all(center),
            right: parse_all(right),
        }
    }

    #[test]
    fn parses_all_builtin_identifiers() {
        for raw in [
            "workspace",
            "cwd",
            "git",
            "cpu",
            "memory",
            "network",
            "battery",
            "clock",
        ] {
            assert!(StatusModuleId::parse(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn parses_qualified_provider_identifier() {
        let id = StatusModuleId::parse("example.git:branch-changed").unwrap();
        assert_eq!(id.key(), "example.git:branch-changed");
    }

    #[test]
    fn rejects_unknown_bare_identifier() {
        assert_eq!(
            StatusModuleId::parse("frobnicator"),
            Err(StatusRegistryError::UnknownModule {
                value: "frobnicator".to_string(),
            })
        );
    }

    #[test]
    fn rejects_malformed_qualified_names() {
        for raw in [
            "Example.git:build",
            "example:build",
            "example.git:",
            "example.git:Build",
            "example.git:build me",
        ] {
            assert!(
                matches!(
                    StatusModuleId::parse(raw),
                    Err(StatusRegistryError::InvalidQualifiedName { .. })
                        | Err(StatusRegistryError::UnknownModule { .. })
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn duplicate_membership_across_slots_fails() {
        let slots = slots_of(&["workspace", "cwd"], &["clock"], &["cpu", "cwd"]);
        assert_eq!(
            slots.validate(),
            Err(StatusRegistryError::DuplicateMembership {
                value: "cwd".to_string(),
            })
        );
    }

    #[test]
    fn empty_slots_validate_and_collapse() {
        let slots = StatusSlots::default();
        slots.validate().unwrap();
        assert!(slots.render_order().is_empty());
    }

    #[test]
    fn render_order_is_left_center_right() {
        let slots = slots_of(&["workspace", "cwd"], &["clock"], &["cpu"]);
        slots.validate().unwrap();
        let order: Vec<String> = slots
            .render_order()
            .iter()
            .map(StatusModuleId::key)
            .collect();
        assert_eq!(order, ["workspace", "cwd", "clock", "cpu"]);
    }

    #[test]
    fn segment_truncates_at_char_boundary_with_flag() {
        let long = "é".repeat(STATUS_TEXT_MAX_CHARS + 10);
        let segment = ModuleSegment::new(StatusModuleId::Clock, &long, None, None, 100);
        assert!(segment.truncated);
        assert_eq!(segment.text.chars().count(), STATUS_TEXT_MAX_CHARS);
        assert!(segment.text.is_char_boundary(segment.text.len()));
    }

    #[test]
    fn render_falls_back_without_terminal_truth() {
        let inputs = StatusInputs {
            clock_text: "12:00".to_string(),
            ..StatusInputs::default()
        };
        let workspace = render_module(&StatusModuleId::Workspace, &inputs).unwrap();
        assert_eq!(workspace.text, STATUS_MISSING_TEXT);
        let cpu = render_module(&StatusModuleId::Cpu, &inputs).unwrap();
        assert_eq!(cpu.text, "cpu —");
        // Battery hides when no battery exists.
        assert_eq!(render_module(&StatusModuleId::Battery, &inputs), None);
        let with_battery = StatusInputs {
            battery_percent: Some(80),
            ..inputs
        };
        assert_eq!(
            render_module(&StatusModuleId::Battery, &with_battery)
                .unwrap()
                .text,
            "bat 80%"
        );
    }

    #[test]
    fn render_uses_snapshot_values() {
        let inputs = StatusInputs {
            workspace: Some("ws-1".to_string()),
            git_branch: Some("main".to_string()),
            git_dirty: Some(true),
            cpu_percent: Some(12.4),
            clock_text: "12:00".to_string(),
            ..StatusInputs::default()
        };
        assert_eq!(
            render_module(&StatusModuleId::Workspace, &inputs)
                .unwrap()
                .text,
            "ws-1"
        );
        assert_eq!(
            render_module(&StatusModuleId::Git, &inputs).unwrap().text,
            "main*"
        );
        assert_eq!(
            render_module(&StatusModuleId::Cpu, &inputs).unwrap().text,
            "cpu 12%"
        );
    }
}
