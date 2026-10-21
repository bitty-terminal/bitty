//! Configuration-dashboard key routing and write dispatch
//! (issue #2380, CTX-1429).
//!
//! The panel state machine lives in [`bitty_runtime::runtime::dashboard`];
//! this module is the app half: it maps one physical keypress into the
//! closed dashboard vocabulary, consumes unmodified keys while the panel is
//! open (so nothing leaks into the shell behind a modal surface), and
//! applies the typed intents the panel returns after its in-panel
//! confirmation.
//!
//! # Writes
//!
//! Only two intents exist, and each routes to a mutation path that already
//! exists and is already user-initiated:
//!
//! - [`DashboardAction::PreviewTheme`] → `Runtime::preview_theme_preset`,
//!   the non-persisting live palette path (`bitty.theme.preview`), so a
//!   preview can never rewrite the user's config file;
//! - [`DashboardAction::SetPluginEnabled`] → the managed-manifest
//!   enable/disable the `bitty plugin enable|disable` CLI owns (atomic
//!   write + backup + owner-only mode), with the same pin and grant checks.
//!
//! No file is edited, no shell runs, and no plugin runtime/services are
//! created on any of these paths; settings are read through the bounded
//! plain-config loader.

#![forbid(unsafe_code)]

use bitty_config::KeyRef;
use bitty_platform::{KeyEvent, LogicalKey, NamedKey};
use bitty_runtime::runtime::dashboard::{
    DashboardAction, DashboardKey, DashboardOutcome, DashboardSection,
};

use crate::terminal_app::TerminalApp;

/// Map one keypress into the closed dashboard vocabulary.
///
/// Chorded presses (any of ctrl/alt/super) always return `None`: they stay
/// with the user keymap, so the panel's own toggle chord still closes it and
/// global bindings keep working while the surface is visible.
#[must_use]
pub(crate) fn dashboard_key_for(key: &KeyEvent, keyref: &KeyRef) -> Option<DashboardKey> {
    if keyref.ctrl || keyref.alt || keyref.super_held {
        return None;
    }
    match &key.logical_key {
        LogicalKey::Named(NamedKey::Escape) => Some(DashboardKey::Close),
        LogicalKey::Named(NamedKey::Enter) => Some(DashboardKey::Activate),
        LogicalKey::Named(NamedKey::Tab) if keyref.shift => Some(DashboardKey::PrevSection),
        LogicalKey::Named(NamedKey::Tab) => Some(DashboardKey::NextSection),
        LogicalKey::Named(NamedKey::ArrowUp) => Some(DashboardKey::Up),
        LogicalKey::Named(NamedKey::ArrowDown) => Some(DashboardKey::Down),
        LogicalKey::Named(NamedKey::ArrowLeft) => Some(DashboardKey::PrevSection),
        LogicalKey::Named(NamedKey::ArrowRight) => Some(DashboardKey::NextSection),
        LogicalKey::Named(NamedKey::Home) => Some(DashboardKey::First),
        LogicalKey::Named(NamedKey::End) => Some(DashboardKey::Last),
        LogicalKey::Named(NamedKey::PageUp) => Some(DashboardKey::PageUp),
        LogicalKey::Named(NamedKey::PageDown) => Some(DashboardKey::PageDown),
        _ => {
            let text =
                key.text
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .or(match &key.logical_key {
                        LogicalKey::Character(text) => Some(text.as_str()),
                        _ => None,
                    })?;
            match text.chars().next()? {
                'j' => Some(DashboardKey::Down),
                'k' => Some(DashboardKey::Up),
                'h' => Some(DashboardKey::PrevSection),
                'l' => Some(DashboardKey::NextSection),
                'g' => Some(DashboardKey::First),
                'G' => Some(DashboardKey::Last),
                'q' => Some(DashboardKey::Close),
                'r' => Some(DashboardKey::Reload),
                digit @ '1'..='6' => {
                    // Keep the character jump aligned with the closed section
                    // set; a seventh section must update both in one place.
                    let index = digit as u8 - b'0';
                    (usize::from(index) <= DashboardSection::ALL.len())
                        .then_some(DashboardKey::Jump(index))
                }
                _ => None,
            }
        }
    }
}

impl TerminalApp {
    /// Logs a dashboard refusal on stderr (warn gate aware).
    pub(crate) fn warn_dashboard(&self, message: &str) {
        crate::logging::warn(|| format!("bitty: dashboard: {message}"));
    }

    /// Routes one keypress while the dashboard is open.
    ///
    /// Returns `true` when the key was consumed. Unmodified presses are
    /// always consumed — even when they map to nothing — so a modal
    /// configuration surface never types into the shell behind it. Chorded
    /// presses return `false` and continue through the normal keymap path.
    pub(crate) fn handle_dashboard_key(&mut self, key: &KeyEvent, keyref: &KeyRef) -> bool {
        if keyref.ctrl || keyref.alt || keyref.super_held {
            return false;
        }
        let Some(mapped) = dashboard_key_for(key, keyref) else {
            // Consume the press without effect: no state change, no shell
            // bytes, no repaint needed.
            return true;
        };
        match self.runtime.dashboard_key(mapped) {
            DashboardOutcome::Ignored => {}
            DashboardOutcome::Redraw | DashboardOutcome::Armed | DashboardOutcome::Close => {}
            DashboardOutcome::Reload => self.refresh_dashboard_snapshot(),
            DashboardOutcome::Activate(action) => self.apply_dashboard_action(action),
        }
        true
    }

    /// Applies one confirmed dashboard intent, then refreshes the snapshot.
    ///
    /// A refused write is logged and leaves state untouched; the refreshed
    /// snapshot then shows the real (unchanged) state instead of the stale
    /// row.
    pub(crate) fn apply_dashboard_action(&mut self, action: DashboardAction) {
        match action {
            DashboardAction::None => {}
            DashboardAction::PreviewTheme { name } => {
                match self.runtime.preview_theme_preset(&name) {
                    Ok(_changed) => {
                        self.dashboard_theme_name.clone_from(&name);
                        if let Some(window) = self.window.handle.as_ref() {
                            window.request_redraw();
                        }
                    }
                    Err(error) => {
                        self.warn_dashboard(&format!("theme preview refused ({error})"));
                    }
                }
            }
            DashboardAction::SetPluginEnabled { .. } if self.safe_mode => {
                self.warn_dashboard("safe mode is read-only; plugin state was not changed");
            }
            DashboardAction::SetPluginEnabled { id, enabled } => {
                let Some(path) = self
                    .dashboard_sources
                    .as_ref()
                    .and_then(|sources| sources.plugin_state_path.clone())
                else {
                    self.warn_dashboard("plugin state path unavailable; nothing was written");
                    return;
                };
                match crate::plugin::dashboard_set_enabled(&path, &id, enabled) {
                    Ok(changed) => crate::logging::info(|| {
                        format!(
                            "bitty: dashboard: '{}' {}",
                            id,
                            if changed {
                                if enabled { "enabled" } else { "disabled" }
                            } else {
                                "already in the requested state"
                            }
                        )
                    }),
                    Err(error) => self.warn_dashboard(&error),
                }
            }
        }
        self.refresh_dashboard_snapshot();
        if let Some(window) = self.window.handle.as_ref() {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_platform::{KeyLocation, PressState};

    fn key_event(logical_key: LogicalKey, text: Option<&str>) -> KeyEvent {
        KeyEvent {
            logical_key,
            text: text.map(str::to_string),
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        }
    }

    fn keyref() -> KeyRef {
        KeyRef {
            key: bitty_config::KeyName::Char('j'),
            shift: false,
            ctrl: false,
            alt: false,
            super_held: false,
        }
    }

    #[test]
    fn letter_keys_map_to_navigation() {
        let reference = KeyRef {
            key: bitty_config::KeyName::Char('j'),
            ..keyref()
        };
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Character("j".into()), Some("j")),
                &reference
            ),
            Some(DashboardKey::Down)
        );
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Character("q".into()), Some("q")),
                &reference
            ),
            Some(DashboardKey::Close)
        );
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Character("3".into()), Some("3")),
                &reference
            ),
            Some(DashboardKey::Jump(3))
        );
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Character("G".into()), Some("G")),
                &reference
            ),
            Some(DashboardKey::Last)
        );
    }

    #[test]
    fn named_keys_map_and_shift_tab_reverses() {
        let reference = keyref();
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Named(NamedKey::Enter), None),
                &reference
            ),
            Some(DashboardKey::Activate)
        );
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Named(NamedKey::Escape), None),
                &reference
            ),
            Some(DashboardKey::Close)
        );
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Named(NamedKey::PageDown), None),
                &reference
            ),
            Some(DashboardKey::PageDown)
        );
        let shifted = KeyRef {
            shift: true,
            ..reference
        };
        assert_eq!(
            dashboard_key_for(&key_event(LogicalKey::Named(NamedKey::Tab), None), &shifted),
            Some(DashboardKey::PrevSection)
        );
    }

    #[test]
    fn chorded_presses_stay_with_the_user_keymap() {
        let reference = KeyRef {
            alt: true,
            ..keyref()
        };
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Character("j".into()), Some("j")),
                &reference
            ),
            None
        );
        let reference = keyref();
        assert_eq!(
            dashboard_key_for(
                &key_event(LogicalKey::Named(NamedKey::Space), None),
                &reference
            ),
            None
        );
    }
}
