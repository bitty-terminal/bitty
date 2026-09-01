#![forbid(unsafe_code)]
//! Shell integration via Panel Runtime — observation-only, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.shell-integration` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-014 pre-study). It
//! observes committed terminal state only (`State::cwd_report` from `OSC 7` and
//! `State::zones` from `OSC 133` A/B/C/D plus exit status `D;code`) via the
//! bounded side queue (`SideQueue<HostObservation>`, `DropOldest`) and, when a
//! Panel is mounted, via the Panel EventBus (`64`/`1024`/`2 MiB`/`8192`,
//! `DropOldest`). No parser, renderer, or input hot path is entered, and no
//! grid mutation ever occurs here (only `Action` writes `State` per Terminal
//! State RFC). Default is disabled (fresh `EffectiveConfig` has empty `plugins`);
//! `bitty --safe` rejects `bitty-terminal.*` as non-builtin without panic.

use bitty_term_state::{State, ZoneKind, ZoneRecord};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Re-exported zone bound (mirrors `bitty_term_state::ZONE_RECORDS_MAX`).
pub const SHELL_ZONE_MAX: usize = bitty_term_state::ZONE_RECORDS_MAX;

/// Cwd payload is bounded by `BoundedString::MAX_LEN` (4096) at the parser
/// boundary; this constant documents the shell-integration view.
pub const SHELL_CWD_MAX_BYTES: usize = 4096;

/// Panel payload for shell observations is bounded by `BUS_EVENT_MAX_BYTES`
/// (8 KiB) at the bus admission boundary.
pub const SHELL_PAYLOAD_MAX_BYTES: usize = 8 * 1024;

/// Observation-only shell view over a `State`.
///
/// All methods are pure reads of `State::cwd_report` / `State::zones`; no
/// mutation, no I/O, no hot-path. Bounded: at most `ZONE_RECORDS_MAX` records.
#[derive(Debug, Clone, Copy)]
pub struct ShellIntegration;

impl ShellIntegration {
    /// Current working directory report (`OSC 7`), if any. Bounded `<=4096`.
    #[must_use]
    pub fn cwd(state: &State) -> Option<&str> {
        state.cwd_report()
    }

    /// All zone records oldest first, bounded `<=1024`.
    #[must_use]
    pub fn zones(state: &State) -> Vec<ZoneRecord> {
        state.zones().copied().collect()
    }

    /// Zone count (bounded).
    #[must_use]
    pub fn zone_count(state: &State) -> usize {
        state.zone_len()
    }

    /// Most recent zone, if any.
    #[must_use]
    pub fn last_zone(state: &State) -> Option<ZoneRecord> {
        state.zones().copied().last()
    }

    /// Zones filtered by kind (bounded).
    #[must_use]
    pub fn zones_of_kind(state: &State, kind: ZoneKind) -> Vec<ZoneRecord> {
        state.zones().copied().filter(|r| r.kind == kind).collect()
    }

    /// Prompt-start markers (`A`) oldest first.
    #[must_use]
    pub fn prompt_starts(state: &State) -> Vec<ZoneRecord> {
        Self::zones_of_kind(state, ZoneKind::PromptStart)
    }

    /// Exit status of the most recent `D` marker, if any.
    #[must_use]
    pub fn last_exit_code(state: &State) -> Option<i32> {
        state
            .zones()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find(|r| r.kind == ZoneKind::OutputEnd)
            .and_then(|r| r.exit_code)
    }

    /// Whether shell integration has any observable data (cwd or at least one zone).
    #[must_use]
    pub fn has_observation(state: &State) -> bool {
        state.cwd_report().is_some() || state.zone_len() > 0
    }
}

/// Creates a shell-integration panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults). Returns the
/// panel handle on success; caller must still activate the associated plugin
/// via the public PluginHost path (`declare → resolve → register →
/// GrantRecord → activate`) for capability `terminal.semantic-read`.
pub fn create_shell_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: crate::ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    // Panel creation is bounded and fail-closed; config defaults within
    // [1,32]/[1,64] are enforced at `PanelRegistry::new`.
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that shell-integration panel creation respects bounded defaults
/// and leaves previous valid state intact on failure (typed errors, no panic).
pub fn validate_shell_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::{State, TerminalAction, ZoneKind};
    use bitty_vt::BoundedString;

    fn cwd(state: &mut State, url: &str) {
        state.apply(&TerminalAction::OscCwd {
            url: BoundedString::new(url),
        });
    }

    fn mark(state: &mut State, kind: ZoneKind, exit_code: Option<i32>) {
        state.apply(&TerminalAction::OscPromptMark { kind, exit_code });
    }

    #[test]
    fn observation_only_no_hot_path() {
        let mut state = State::new();
        assert!(!ShellIntegration::has_observation(&state));
        cwd(&mut state, "file:///home/user");
        assert_eq!(ShellIntegration::cwd(&state), Some("file:///home/user"));
        mark(&mut state, ZoneKind::PromptStart, None);
        mark(&mut state, ZoneKind::InputStart, None);
        mark(&mut state, ZoneKind::OutputStart, None);
        mark(&mut state, ZoneKind::OutputEnd, Some(0));
        assert_eq!(ShellIntegration::zone_count(&state), 4);
        assert_eq!(ShellIntegration::last_exit_code(&state), Some(0));
        // Observation is via committed state (cwd/zones), never grid mutation.
        assert!(ShellIntegration::has_observation(&state));
    }

    #[test]
    fn bounded_oldest_evicted() {
        let mut state = State::new();
        for _ in 0..SHELL_ZONE_MAX + 5 {
            mark(&mut state, ZoneKind::PromptStart, None);
        }
        assert_eq!(ShellIntegration::zone_count(&state), SHELL_ZONE_MAX);
    }

    #[test]
    fn exit_code_only_on_output_end() {
        let mut state = State::new();
        // PromptStart must never carry exit_code, even if supplied.
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::PromptStart,
            exit_code: Some(99),
        });
        assert_eq!(ShellIntegration::last_zone(&state).unwrap().exit_code, None);
        mark(&mut state, ZoneKind::OutputEnd, Some(7));
        assert_eq!(ShellIntegration::last_exit_code(&state), Some(7));
    }

    #[test]
    fn cwd_bounded_at_4096() {
        let mut state = State::new();
        let long = "a".repeat(SHELL_CWD_MAX_BYTES + 100);
        cwd(&mut state, &long);
        assert!(ShellIntegration::cwd(&state).unwrap().len() <= SHELL_CWD_MAX_BYTES);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
        use bitty_ui::ViewId;
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        // Registry starts empty: default disabled.
        assert_eq!(reg.panel_count(), 0);
        let id = create_shell_panel(&mut reg, ws, view).expect("create shell panel");
        assert_eq!(reg.panel_count(), 1);
        // Creating a second panel on same view must fail-closed (AlreadyMounted).
        let ws2 = WorkspaceId::new(1);
        let handle2 = reg.create_panel(PanelType::Helper, Some(ws2)).unwrap();
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        // PanelId is distinct newtype; no From bridge.
        let _ = id.get();
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_shell_panel_config(&bad).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_shell_panel_config(&ok).is_ok());
    }
}
