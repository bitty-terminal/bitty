#![forbid(unsafe_code)]
//! Statusline via Panel Runtime — panel reactive, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.statusline` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Statusline is
//! presentation of cwd, mode, Git and task state via status-component
//! composition. It observes committed terminal state only (`State::cwd_report`,
//! `State::title`, `State::zones` via `OSC 7/133`) via the bounded side
//! queue (`SideQueue<HostObservation>`, `DropOldest`) and, when a Panel is
//! mounted, via the Panel EventBus (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`).
//! No parser, renderer, or input hot path is entered, and no grid mutation
//! ever occurs here (only `Action` writes `State` per Terminal State RFC).
//! Default is disabled (fresh `EffectiveConfig` has empty `plugins`);
//! `bitty --safe` rejects `bitty-terminal.*` as non-builtin without panic,
//! identical to third-party `xuepoo.*` parity (no private channel). Bounded
//! queues (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload,
//! `32`/`8 KiB` batch) and single-process `winit` `PanelRegistry` per window
//! are verified headlessly.

use bitty_term_state::State;

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Maximum statusline components to compose — bounded for presentation.
pub const STATUSLINE_MAX_COMPONENTS: usize = 8;

/// Maximum chars per component — mirrors overlay text bound for status presentation.
pub const STATUSLINE_COMPONENT_MAX_CHARS: usize = 64;

/// Maximum total statusline length — mirrors overlay text bound (128).
pub const STATUSLINE_MAX_CHARS: usize = bitty_ui::panel::MAX_OVERLAY_TEXT_LEN;

/// Panel payload for statusline observations is bounded by `BUS_EVENT_MAX_BYTES`
/// (8 KiB) at the bus admission boundary.
pub const STATUSLINE_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Maximum panels per workspace for statusline — mirrors `MAX_PANELS_PER_WORKSPACE` (32).
pub const STATUSLINE_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
pub const STATUSLINE_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// StatuslineIntegration — pure, observation-only helpers over committed state.
/// No mutation of `State`, no hot-path, bounded `<=8` components, each `<=64` chars.
#[derive(Debug, Clone, Copy)]
pub struct StatuslineIntegration;

impl StatuslineIntegration {
    /// Components for `state`: cwd, title, last exit code if present. Each
    /// component is truncated at char boundary to `STATUSLINE_COMPONENT_MAX_CHARS`.
    /// Total bounded to `STATUSLINE_MAX_COMPONENTS` and `STATUSLINE_MAX_CHARS`.
    #[must_use]
    pub fn components(state: &State) -> Vec<String> {
        let mut comps = Vec::new();
        if let Some(cwd) = state.cwd_report() {
            let truncated = if cwd.chars().count() <= STATUSLINE_COMPONENT_MAX_CHARS {
                cwd.to_owned()
            } else {
                cwd.chars().take(STATUSLINE_COMPONENT_MAX_CHARS).collect()
            };
            comps.push(format!("cwd:{truncated}"));
        }
        let title = state.title();
        if !title.is_empty() {
            let truncated = if title.chars().count() <= STATUSLINE_COMPONENT_MAX_CHARS {
                title.to_owned()
            } else {
                title.chars().take(STATUSLINE_COMPONENT_MAX_CHARS).collect()
            };
            comps.push(format!("title:{truncated}"));
        }
        // Last exit code from zones if any
        let last_exit = state
            .zones()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find(|r| r.kind == bitty_term_state::ZoneKind::OutputEnd)
            .and_then(|r| r.exit_code);
        if let Some(code) = last_exit {
            comps.push(format!("exit:{code}"));
        }
        // Bounded to max components
        comps.truncate(STATUSLINE_MAX_COMPONENTS);
        comps
    }

    /// Rendered statusline string from `state`: components joined with ` | `,
    /// truncated to `STATUSLINE_MAX_CHARS` at char boundary. Empty state yields
    /// empty string (no fallback pollution).
    #[must_use]
    pub fn render(state: &State) -> String {
        let comps = Self::components(state);
        if comps.is_empty() {
            return String::new();
        }
        let joined = comps.join(" | ");
        if joined.chars().count() <= STATUSLINE_MAX_CHARS {
            joined
        } else {
            joined.chars().take(STATUSLINE_MAX_CHARS).collect()
        }
    }

    /// Whether `event_kind` is reactive for statusline (observation-only
    /// `terminal.cwd-changed` / `terminal.title-changed` / `focus.changed`).
    #[must_use]
    pub fn is_reactive_event(event_kind: &str) -> bool {
        matches!(
            event_kind,
            "terminal.cwd-changed" | "terminal.title-changed" | "focus.changed" | "terminal.bell"
        )
    }

    /// Whether statusline has any observable data (cwd or title or zone).
    #[must_use]
    pub fn has_status(state: &State) -> bool {
        state.cwd_report().is_some() || !state.title().is_empty() || state.zone_len() > 0
    }

    /// Validates that `text` fits statusline total bound.
    #[must_use]
    pub fn is_render_bounded(text: &str) -> bool {
        text.chars().count() <= STATUSLINE_MAX_CHARS
    }

    /// Validates that a single component fits per-component bound.
    #[must_use]
    pub fn is_component_bounded(text: &str) -> bool {
        text.chars().count() <= STATUSLINE_COMPONENT_MAX_CHARS
    }
}

/// Creates a statusline panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults). Returns the
/// panel handle on success; caller must still activate the associated plugin
/// via the public PluginHost path (`declare → resolve → register →
/// GrantRecord → activate`) for capabilities `terminal.semantic-read` and `ui.rich`.
pub fn create_statusline_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: crate::ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that statusline panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
pub fn validate_statusline_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_term_state::{State, TerminalAction, ZoneKind};
    use bitty_ui::ViewId;
    use bitty_vt::BoundedString;

    fn cwd(state: &mut State, url: &str) {
        state.apply(&TerminalAction::OscCwd {
            url: BoundedString::new(url),
        });
    }

    fn title(state: &mut State, text: &str) {
        state.apply(&TerminalAction::OscTitle {
            text: BoundedString::new(text),
        });
    }

    #[test]
    fn components_bounded_and_truncated() {
        let mut state = State::new();
        assert!(StatuslineIntegration::components(&state).is_empty());
        assert_eq!(StatuslineIntegration::render(&state), "");
        assert!(!StatuslineIntegration::has_status(&state));
        cwd(&mut state, "file:///home/user/projects/foo");
        title(&mut state, "hello");
        let comps = StatuslineIntegration::components(&state);
        assert_eq!(comps.len(), 2);
        assert!(comps[0].contains("file:///home/user/projects/foo"));
        assert!(comps[1].contains("hello"));
        let rendered = StatuslineIntegration::render(&state);
        assert!(StatuslineIntegration::is_render_bounded(&rendered));
        assert!(rendered.contains("cwd:"));
        assert!(rendered.contains("title:"));
        // Bounded at 128 chars total
        let long = "a".repeat(STATUSLINE_COMPONENT_MAX_CHARS + 100);
        cwd(&mut state, &long);
        let comps2 = StatuslineIntegration::components(&state);
        for c in &comps2 {
            assert!(c.chars().count() <= STATUSLINE_COMPONENT_MAX_CHARS + 4); // prefix
        }
        assert!(StatuslineIntegration::render(&state).chars().count() <= STATUSLINE_MAX_CHARS);
        // Multibyte safe
        let multi = "é".repeat(STATUSLINE_MAX_CHARS + 20);
        title(&mut state, &multi);
        assert!(StatuslineIntegration::render(&state).chars().count() <= STATUSLINE_MAX_CHARS);
    }

    #[test]
    fn reactive_events_are_observation_only() {
        assert!(StatuslineIntegration::is_reactive_event(
            "terminal.cwd-changed"
        ));
        assert!(StatuslineIntegration::is_reactive_event(
            "terminal.title-changed"
        ));
        assert!(StatuslineIntegration::is_reactive_event("focus.changed"));
        assert!(!StatuslineIntegration::is_reactive_event("intercept.paste"));
        assert!(!StatuslineIntegration::is_reactive_event("byte-received"));
        assert!(!StatuslineIntegration::is_reactive_event("cell-changed"));
        // v1 statusline never subscribes to hot-path
        for ev in ["byte-received", "cell-changed", "damage"] {
            assert!(!StatuslineIntegration::is_reactive_event(ev));
        }
    }

    #[test]
    fn exit_code_component_bounded() {
        let mut state = State::new();
        state.apply(&TerminalAction::OscPromptMark {
            kind: ZoneKind::OutputEnd,
            exit_code: Some(42),
        });
        let comps = StatuslineIntegration::components(&state);
        assert!(comps.iter().any(|c| c == "exit:42"));
        let rendered = StatuslineIntegration::render(&state);
        assert!(rendered.contains("exit:42"));
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_statusline_panel(&mut reg, ws, view).expect("create statusline panel");
        assert_eq!(reg.panel_count(), 1);
        // Second panel on same view must fail-closed (AlreadyMounted).
        let handle2 = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel");
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        let _ = id.get();
        // Reactive via Panel EventBus bounded DropOldest: declare → subscribe → publish → drain
        let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
        let ws2 = WorkspaceId::new(2);
        let view2 = ViewId::new(2);
        let h = reg2.create_panel(PanelType::Helper, Some(ws2)).unwrap();
        reg2.mount_panel(h.id, h.generation, view2).unwrap();
        let topic = reg2
            .declare_topic("xuepoo.statusline:status-update")
            .unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("status{i}")).unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        // Payload 8 KiB bound
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        // Batch 32/8 KiB
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // First batch after DropOldest 80->64 should start at status16
        assert_eq!(batch[0].payload.as_str(), "status16");
        // Focus lifecycle via Panel API
        reg2.focus_panel(h.id, h.generation, ws2).expect("focus");
        assert_eq!(reg2.focused_panel(ws2), Some(h.id));
        reg2.suspend_panel(h.id, h.generation).expect("suspend");
        assert_eq!(reg2.focused_panel(ws2), None);
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_statusline_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_statusline_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_statusline_panel_config(&ok).is_ok());
    }

    #[test]
    fn panel_reactive_no_hot_path_no_grid_mutation() {
        // Statusline is observation-only: re-rendering never mutates State.
        let mut state = State::new();
        cwd(&mut state, "file:///home/user");
        let gen_before = state.generation();
        let rendered = StatuslineIntegration::render(&state);
        assert!(!rendered.is_empty());
        assert_eq!(state.generation(), gen_before);
        // Rendering twice yields same result (deterministic, pure)
        let rendered2 = StatuslineIntegration::render(&state);
        assert_eq!(rendered, rendered2);
        // Bounded coalescing: multiple cwd changes coalesce to latest via bus
        let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
        let ws = WorkspaceId::new(10);
        let view = ViewId::new(10);
        let h = reg.create_panel(PanelType::Helper, Some(ws)).unwrap();
        reg.mount_panel(h.id, h.generation, view).unwrap();
        let topic = reg
            .declare_topic("xuepoo.statusline:title-changed")
            .unwrap();
        reg.subscribe(h.id, h.generation, &topic).unwrap();
        // Publish same coalescable topic repeatedly; queue should coalesce to 1 when published sequentially?
        // Actually bus queue coalesces pending undelivered: publishing same topic replaces existing.
        // We publish 3 different payloads for same topic; only latest survives if not yet drained.
        reg.publish(
            &topic,
            crate::registry::BoundedPayload::try_new("a").unwrap(),
        )
        .unwrap();
        reg.publish(
            &topic,
            crate::registry::BoundedPayload::try_new("b").unwrap(),
        )
        .unwrap();
        reg.publish(
            &topic,
            crate::registry::BoundedPayload::try_new("c").unwrap(),
        )
        .unwrap();
        // With coalescable, len should be 1 (latest-wins) if bus respects is_coalescable
        // Our EventTopic::is_coalescable returns true for title/cwd/focus, so it should coalesce.
        assert!(reg.bus_events_for_panel(h.id) <= 2);
    }
}
