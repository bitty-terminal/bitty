//! Reload classification and reconcile helpers.
//!
//! Implements the RFC section “Reload classification”: every schema change
//! from a reloaded plan lands in exactly one class:
//!
//! - [`ReloadClass::Live`] — applied by diff-and-reconcile without restart,
//! - [`ReloadClass::RestartRequired`] — accepted and persisted, effective
//!   after next process start,
//! - [`ReloadClass::Rejected`] — validation failure; previous good plan
//!   remains active, diagnostics emitted.
//!
//! Classification is declared by the schema, never inferred at runtime. A
//! reload containing any `RestartRequired` change reports that fact up front,
//! and reload reuses the same validation/merge path as startup.
//!
//! # Drift note
//!
//! The per-field table here is draft. It will move to the authoritative
//! home once the schema stabilizes (see RFC open item “Per-field reload
//! classification”). Changing a field's class is a contract change and needs
//! a test update.

use crate::error::ConfigError;
use crate::types::EffectiveConfig;

/// Classification for a single field change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadClass {
    /// Applied live by diff-and-reconcile.
    Live,
    /// Accepted and persisted; effective after next start.
    RestartRequired,
    /// Validation failure; previous good plan stays active.
    Rejected,
}

impl std::fmt::Display for ReloadClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Live => "live-reconcilable",
            Self::RestartRequired => "restart-required",
            Self::Rejected => "rejected",
        };
        f.write_str(s)
    }
}

/// Declared per-field classification (draft table).
///
/// | Field                     | Class              |
/// |---------------------------|--------------------|
/// | `font.family`             | Live               |
/// | `font.size`               | Live               |
/// | `font.line_height`        | Live               |
/// | `font.letter_spacing`     | Live               |
/// | `window.opacity`          | Live               |
/// | `window.padding`          | Live               |
/// | `appearance.theme`        | Live               |
/// | `keymaps`                 | Live               |
/// | `terminal.scrollback`     | RestartRequired    |
/// | `terminal.shell`          | RestartRequired    |
/// | `terminal.scroll_lines_per_notch` | RestartRequired |
/// | `terminal.scroll_pixels_per_notch` | RestartRequired |
/// | `plugins`                 | RestartRequired    |
/// | unknown / undeclared      | Rejected           |
#[must_use]
pub fn classify_field(field: &str) -> ReloadClass {
    match field {
        "font.family"
        | "font.size"
        | "font.line_height"
        | "font.letter_spacing"
        | "font"
        | "window.opacity"
        | "window.padding"
        | "window"
        | "appearance.theme"
        | "appearance"
        | "keymaps" => ReloadClass::Live,
        "terminal.scrollback"
        | "terminal.shell"
        | "terminal.scroll_lines_per_notch"
        | "terminal.scroll_pixels_per_notch"
        | "terminal"
        | "plugins" => ReloadClass::RestartRequired,
        _ => ReloadClass::Rejected,
    }
}

/// A single field diff between two effective configs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    /// Dotted field path.
    pub field: String,
    /// Classification for this field.
    pub class: ReloadClass,
    /// Previous value description (truncated, developer-facing).
    pub before: String,
    /// New value description.
    pub after: String,
}

/// Result of diffing two effective configs for reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// Overall classification: the most severe among all field diffs.
    /// `Rejected` > `RestartRequired` > `Live`. Empty diff is `Live`.
    pub overall: ReloadClass,
    /// Per-field diffs.
    pub diffs: Vec<FieldDiff>,
    /// Whether any field is restart-required.
    pub needs_restart: bool,
    /// Whether any field was rejected (validation should have already failed
    /// in that case; this is a secondary guard).
    pub has_rejected: bool,
}

impl ReloadReport {
    /// `true` if the reload can be applied live without restart.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.overall == ReloadClass::Live
    }
}

/// Compute a diff between `old` and `new` effective configs.
///
/// This is pure data comparison; no process state is touched. Validation of
/// `new` is performed first — if it fails the report is `Rejected`.
pub fn diff(old: &EffectiveConfig, new: &EffectiveConfig) -> ReloadReport {
    // Fast path: if new is invalid, entire reload is rejected.
    if let Err(e) = new.validate() {
        return ReloadReport {
            overall: ReloadClass::Rejected,
            diffs: vec![FieldDiff {
                field: e.field().unwrap_or("unknown").to_string(),
                class: ReloadClass::Rejected,
                before: String::new(),
                after: e.to_string(),
            }],
            needs_restart: false,
            has_rejected: true,
        };
    }

    let mut diffs = Vec::new();

    let mut push_if_changed = |field: &str, before: String, after: String| {
        if before != after {
            let class = classify_field(field);
            diffs.push(FieldDiff {
                field: field.to_string(),
                class,
                before,
                after,
            });
        }
    };

    push_if_changed(
        "font.family",
        old.font.family.clone(),
        new.font.family.clone(),
    );
    push_if_changed(
        "font.size",
        format!("{:.2}", old.font.size),
        format!("{:.2}", new.font.size),
    );
    push_if_changed(
        "font.line_height",
        format!("{:.3}", old.font.line_height),
        format!("{:.3}", new.font.line_height),
    );
    push_if_changed(
        "font.letter_spacing",
        format!("{:.3}", old.font.letter_spacing),
        format!("{:.3}", new.font.letter_spacing),
    );
    push_if_changed(
        "window.opacity",
        format!("{:.3}", old.window.opacity),
        format!("{:.3}", new.window.opacity),
    );
    push_if_changed(
        "window.padding",
        old.window.padding.to_string(),
        new.window.padding.to_string(),
    );
    push_if_changed(
        "terminal.scrollback",
        old.terminal.scrollback.to_string(),
        new.terminal.scrollback.to_string(),
    );
    push_if_changed(
        "terminal.shell",
        format!("{:?}", old.terminal.shell),
        format!("{:?}", new.terminal.shell),
    );
    // CTX-0185: scroll speed is terminal-table state; like scrollback it is
    // adopted at startup (RuntimeConfig is built once from the effective
    // config), so changes are restart-required, not live.
    push_if_changed(
        "terminal.scroll_lines_per_notch",
        old.terminal.scroll_lines_per_notch.to_string(),
        new.terminal.scroll_lines_per_notch.to_string(),
    );
    push_if_changed(
        "terminal.scroll_pixels_per_notch",
        old.terminal.scroll_pixels_per_notch.to_string(),
        new.terminal.scroll_pixels_per_notch.to_string(),
    );
    push_if_changed(
        "appearance.theme",
        format!("{:?}", old.appearance.theme),
        format!("{:?}", new.appearance.theme),
    );
    // Keymaps and plugins: compare sorted ids, not raw order (merge already
    // sorts them).
    let old_kms: Vec<String> = old.keymaps.iter().map(|k| k.id()).collect();
    let new_kms: Vec<String> = new.keymaps.iter().map(|k| k.id()).collect();
    push_if_changed("keymaps", format!("{old_kms:?}"), format!("{new_kms:?}"));
    let old_pls: Vec<String> = old.plugins.iter().map(|p| p.id.clone()).collect();
    let new_pls: Vec<String> = new.plugins.iter().map(|p| p.id.clone()).collect();
    push_if_changed("plugins", format!("{old_pls:?}"), format!("{new_pls:?}"));

    let needs_restart = diffs
        .iter()
        .any(|d| d.class == ReloadClass::RestartRequired);
    let has_rejected = diffs.iter().any(|d| d.class == ReloadClass::Rejected);

    let overall = if has_rejected {
        ReloadClass::Rejected
    } else if needs_restart {
        ReloadClass::RestartRequired
    } else {
        ReloadClass::Live
    };

    ReloadReport {
        overall,
        diffs,
        needs_restart,
        has_rejected,
    }
}

/// Reconcile helper: apply a `Live`-only diff to `current` in place.
///
/// Returns `Err` if the diff contains any `RestartRequired` or `Rejected`
/// field — the caller must defer to next start or retain the last good plan
/// per R-009.
///
/// This helper never performs I/O; the caller owns when to persist the new
/// effective config.
pub fn reconcile_live(
    current: &mut EffectiveConfig,
    new: &EffectiveConfig,
) -> Result<ReloadReport, ConfigError> {
    let report = diff(current, new);
    if report.has_rejected {
        return Err(ConfigError::ReloadRejected {
            message: "reload contains rejected fields".into(),
        });
    }
    if report.needs_restart {
        return Err(ConfigError::ReloadRejected {
            message: "reload contains restart-required changes; persist and restart".into(),
        });
    }
    // Live diffs are safe to apply. New is already validated.
    *current = new.clone();
    Ok(report)
}

/// Safe-mode fallback: the minimal built-in configuration that always starts
/// regardless of external configuration health (`bitty --safe`, R-009).
///
/// This is exactly `EffectiveConfig::default()` — no external layers applied.
#[must_use]
pub fn fallback_builtin() -> EffectiveConfig {
    EffectiveConfig::default()
}

/// Whether a report means the previous good plan should be retained (R-009).
#[must_use]
pub fn should_retain_previous(report: &ReloadReport) -> bool {
    report.has_rejected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FontConfig, TerminalConfig};

    fn cfg_with_scrollback(n: u32) -> EffectiveConfig {
        EffectiveConfig {
            terminal: TerminalConfig {
                scrollback: n,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn diff_live_field() {
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.font = FontConfig {
            family: "JetBrains".into(),
            size: 14.0,
            ..Default::default()
        };
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(!r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "font.family"));
    }

    #[test]
    fn diff_spacing_fields_are_live() {
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.font.line_height = 1.0;
        new.font.letter_spacing = 0.0;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(r.diffs.iter().any(|d| d.field == "font.line_height"));
        assert!(r.diffs.iter().any(|d| d.field == "font.letter_spacing"));
    }

    #[test]
    fn diff_restart_required() {
        let old = cfg_with_scrollback(10_000);
        let new = cfg_with_scrollback(50_000);
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
    }

    #[test]
    fn diff_empty_is_live() {
        let c = EffectiveConfig::default();
        let r = diff(&c, &c);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(r.diffs.is_empty());
    }

    #[test]
    fn diff_rejected_on_invalid_new() {
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.font.size = f32::NAN;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Rejected);
        assert!(r.has_rejected);
    }

    #[test]
    fn reconcile_live_rejects_restart() {
        let mut cur = cfg_with_scrollback(10_000);
        let new = cfg_with_scrollback(50_000);
        assert!(reconcile_live(&mut cur, &new).is_err());
        // cur unchanged
        assert_eq!(cur.terminal.scrollback, 10_000);
    }

    #[test]
    fn reconcile_live_applies_live() {
        let mut cur = EffectiveConfig::default();
        let mut new = cur.clone();
        new.font.family = "Mono".into();
        // Need to ensure new is Live-only diff; changing family is Live.
        // Also need to avoid restart fields: keep same scrollback.
        let r = reconcile_live(&mut cur, &new).expect("live must reconcile");
        assert_eq!(r.overall, ReloadClass::Live);
        assert_eq!(cur.font.family, "Mono");
    }

    #[test]
    fn fallback_is_default() {
        assert_eq!(fallback_builtin(), EffectiveConfig::default());
    }

    #[test]
    fn classify_table() {
        assert_eq!(classify_field("font.family"), ReloadClass::Live);
        assert_eq!(classify_field("font.line_height"), ReloadClass::Live);
        assert_eq!(classify_field("font.letter_spacing"), ReloadClass::Live);
        assert_eq!(
            classify_field("terminal.scrollback"),
            ReloadClass::RestartRequired
        );
        // CTX-0185: scroll speed is restart-required (adopted at startup).
        assert_eq!(
            classify_field("terminal.scroll_lines_per_notch"),
            ReloadClass::RestartRequired
        );
        assert_eq!(
            classify_field("terminal.scroll_pixels_per_notch"),
            ReloadClass::RestartRequired
        );
        assert_eq!(classify_field("bogus"), ReloadClass::Rejected);
    }

    #[test]
    fn diff_scroll_speed_is_restart_required() {
        // CTX-0185: changing either scroll key must surface as a
        // restart-required diff (no silent no-op on reload).
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.terminal.scroll_lines_per_notch = 6;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(
            r.diffs
                .iter()
                .any(|d| d.field == "terminal.scroll_lines_per_notch")
        );
        let mut new2 = old.clone();
        new2.terminal.scroll_pixels_per_notch = 32;
        let r2 = diff(&old, &new2);
        assert_eq!(r2.overall, ReloadClass::RestartRequired);
        assert!(
            r2.diffs
                .iter()
                .any(|d| d.field == "terminal.scroll_pixels_per_notch")
        );
    }
}
