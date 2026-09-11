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
/// | `window.radius_px`        | Live               |
/// | `decoration.gaps_in`      | Live               |
/// | `decoration.gaps_out`     | Live               |
/// | `decoration.border`       | Live               |
/// | `decoration.radius`       | Live               |
/// | `decoration.content_inset`| Live               |
/// | `appearance.theme`        | Live               |
/// | `mod_key`                 | Live               |
/// | `keymaps`                 | Live               |
/// | `terminal.scrollback`     | RestartRequired    |
/// | `terminal.shell`          | RestartRequired    |
/// | `terminal.scroll_lines_per_notch` | RestartRequired |
/// | `terminal.scroll_pixels_per_notch` | RestartRequired |
/// | `selection.auto_copy`     | RestartRequired    |
/// | `layout.gaps_in`          | RestartRequired    |
/// | `layout.gaps_out`         | RestartRequired    |
/// | `scrollbar.mode`          | RestartRequired    |
/// | `scrollbar.width`         | RestartRequired    |
/// | `mouse.focus_follows_mouse` | RestartRequired  |
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
        | "window.radius_px"
        | "window"
        | "decoration.gaps_in"
        | "decoration.gaps_out"
        | "decoration.border"
        | "decoration.radius"
        | "decoration.content_inset"
        | "decoration"
        | "appearance.theme"
        | "appearance"
        | "mod_key"
        | "keymaps" => ReloadClass::Live,
        "terminal.scrollback"
        | "terminal.shell"
        | "terminal.scroll_lines_per_notch"
        | "terminal.scroll_pixels_per_notch"
        | "terminal"
        | "selection.auto_copy"
        | "selection"
        | "layout.gaps_in"
        | "layout.gaps_out"
        | "layout"
        | "scrollbar.mode"
        | "scrollbar.width"
        | "scrollbar"
        | "mouse.focus_follows_mouse"
        | "mouse"
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
    // CTX-0241 S0: radius is a parsed no-op (stored + reported, zero render
    // effect), so changes reconcile live without restart.
    push_if_changed(
        "window.radius_px",
        old.window.radius_px.to_string(),
        new.window.radius_px.to_string(),
    );
    // CTX-0292: Core-owned decoration is validated + stored live (the
    // runtime `set_decoration` path adopts it without restart), so changes
    // reconcile live like `window.radius_px`.
    push_if_changed(
        "decoration.gaps_in",
        old.decoration.gaps_in.to_string(),
        new.decoration.gaps_in.to_string(),
    );
    push_if_changed(
        "decoration.gaps_out",
        old.decoration.gaps_out.to_string(),
        new.decoration.gaps_out.to_string(),
    );
    push_if_changed(
        "decoration.border",
        old.decoration.border.to_string(),
        new.decoration.border.to_string(),
    );
    push_if_changed(
        "decoration.radius",
        old.decoration.radius.to_string(),
        new.decoration.radius.to_string(),
    );
    push_if_changed(
        "decoration.content_inset",
        old.decoration.content_inset.to_string(),
        new.decoration.content_inset.to_string(),
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
    // CTX-0191: auto-copy is adopted at startup (RuntimeConfig is built once
    // from the effective config), so changes are restart-required, not live.
    push_if_changed(
        "selection.auto_copy",
        old.selection.auto_copy.to_string(),
        new.selection.auto_copy.to_string(),
    );
    // CTX-0177: gaps are adopted at startup (RuntimeConfig carries them into
    // every layout call), so changes are restart-required, not live.
    push_if_changed(
        "layout.gaps_in",
        old.layout.gaps_in.to_string(),
        new.layout.gaps_in.to_string(),
    );
    push_if_changed(
        "layout.gaps_out",
        old.layout.gaps_out.to_string(),
        new.layout.gaps_out.to_string(),
    );
    // CTX-0181: scrollbar chrome is adopted at startup (RuntimeConfig is
    // built once from the effective config), so changes are
    // restart-required, not live.
    push_if_changed(
        "scrollbar.mode",
        old.scrollbar.mode.as_str().to_string(),
        new.scrollbar.mode.as_str().to_string(),
    );
    push_if_changed(
        "scrollbar.width",
        old.scrollbar.width.to_string(),
        new.scrollbar.width.to_string(),
    );
    // CTX-0260: hover-focus is adopted at startup (RuntimeConfig is built
    // once from the effective config), so changes are restart-required.
    push_if_changed(
        "mouse.focus_follows_mouse",
        old.mouse.focus_follows_mouse.to_string(),
        new.mouse.focus_follows_mouse.to_string(),
    );
    push_if_changed(
        "appearance.theme",
        format!("{:?}", old.appearance.theme),
        format!("{:?}", new.appearance.theme),
    );
    // CTX-0236: the mod rebinds the resolved chrome map, exactly like an
    // explicit keymap edit, so it reconciles live with the keymaps.
    push_if_changed(
        "mod_key",
        old.mod_key.canonical().to_string(),
        new.mod_key.canonical().to_string(),
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
/// This is `EffectiveConfig::default()` with the Core-owned workspace
/// decoration forced to the safe-mode values `0/0/1/0` (CTX-0292; accepted
/// spec CTX-0118 rule 5) regardless of user configuration. Every other field
/// stays at its built-in default and no external layer is applied.
#[must_use]
pub fn fallback_builtin() -> EffectiveConfig {
    EffectiveConfig {
        decoration: crate::types::DecorationConfig::safe(),
        ..EffectiveConfig::default()
    }
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
    fn diff_mod_flip_is_live_and_reconciles() {
        // CTX-0236: flipping the mod rebinds the resolved chrome map like
        // an explicit keymap edit, so it is live-reconcilable, not restart.
        use crate::keymap::ModKey;
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.mod_key = ModKey::Super;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(!r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "mod_key"));
        let mut cur = old;
        reconcile_live(&mut cur, &new).expect("mod flip reconciles live");
        assert_eq!(cur.mod_key, ModKey::Super);
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
    fn fallback_forces_safe_decoration() {
        // CTX-0292/CTX-0333: safe mode inverts decoration to 0/0/1/0/0
        // regardless of the (non-zero) built-in defaults; every other field
        // is default.
        let fallback = fallback_builtin();
        assert_eq!(fallback.decoration, crate::types::DecorationConfig::safe());
        assert_eq!(fallback.decoration.gaps_in, 0);
        assert_eq!(fallback.decoration.gaps_out, 0);
        assert_eq!(fallback.decoration.border, 1);
        assert_eq!(fallback.decoration.radius, 0);
        assert_eq!(fallback.decoration.content_inset, 0);
        assert_ne!(
            fallback.decoration,
            crate::types::DecorationConfig::default()
        );
        assert_eq!(fallback.font, EffectiveConfig::default().font);
        assert_eq!(fallback.window, EffectiveConfig::default().window);
        fallback.validate().expect("safe fallback is valid");
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
        // CTX-0191: auto-copy is restart-required (adopted at startup).
        assert_eq!(
            classify_field("selection.auto_copy"),
            ReloadClass::RestartRequired
        );
        assert_eq!(classify_field("selection"), ReloadClass::RestartRequired);
        // CTX-0177: gaps are restart-required (adopted at startup).
        assert_eq!(
            classify_field("layout.gaps_in"),
            ReloadClass::RestartRequired
        );
        assert_eq!(
            classify_field("layout.gaps_out"),
            ReloadClass::RestartRequired
        );
        assert_eq!(classify_field("layout"), ReloadClass::RestartRequired);
        // CTX-0223: padding/opacity are Live — the running instance adopts
        // them without restart (`Runtime::set_window_padding` re-derives the
        // grid in place; `WindowHandle::set_opacity` retoggles the winit
        // transparency flag). No end-to-end `ctl` trigger yet (same as every
        // other Live field), but the apply path itself needs no restart.
        assert_eq!(classify_field("window.opacity"), ReloadClass::Live);
        assert_eq!(classify_field("window.padding"), ReloadClass::Live);
        // CTX-0241 S0: radius is a parsed no-op, still Live (reconciles
        // without restart, zero render effect).
        assert_eq!(classify_field("window.radius_px"), ReloadClass::Live);
        assert_eq!(classify_field("window"), ReloadClass::Live);
        // CTX-0292: Core-owned decoration is stored + validated live.
        assert_eq!(classify_field("decoration.gaps_in"), ReloadClass::Live);
        assert_eq!(classify_field("decoration.gaps_out"), ReloadClass::Live);
        assert_eq!(classify_field("decoration.border"), ReloadClass::Live);
        assert_eq!(classify_field("decoration.radius"), ReloadClass::Live);
        assert_eq!(
            classify_field("decoration.content_inset"),
            ReloadClass::Live
        );
        assert_eq!(classify_field("decoration"), ReloadClass::Live);
        assert_eq!(classify_field("bogus"), ReloadClass::Rejected);
    }

    #[test]
    fn diff_window_fields_are_live_and_reconcile() {
        // CTX-0223: changing padding/opacity must surface as a Live diff
        // (the dead-knob finding), and reconcile must apply it to the plan.
        // CTX-0241 S0 extends the same Live class to `window.radius_px`
        // (parsed no-op: stored + reported, zero render effect).
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.window.padding = 4;
        new.window.opacity = 0.9;
        new.window.radius_px = 12;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(!r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "window.padding"));
        assert!(r.diffs.iter().any(|d| d.field == "window.opacity"));
        assert!(r.diffs.iter().any(|d| d.field == "window.radius_px"));
        let mut cur = old;
        let applied = reconcile_live(&mut cur, &new).expect("live must reconcile");
        assert_eq!(applied.overall, ReloadClass::Live);
        assert_eq!(cur.window.padding, 4);
        assert!((cur.window.opacity - 0.9).abs() < f32::EPSILON);
        assert_eq!(cur.window.radius_px, 12);
    }

    #[test]
    fn diff_decoration_is_live_and_reconcile() {
        // CTX-0292: every decoration field surfaces as a Live diff and
        // reconciles into the effective config without restart.
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.decoration.gaps_in = 0;
        new.decoration.gaps_out = 0;
        new.decoration.border = 1;
        new.decoration.radius = 0;
        new.decoration.content_inset = 0;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(!r.needs_restart);
        for field in [
            "decoration.gaps_in",
            "decoration.gaps_out",
            "decoration.border",
            "decoration.radius",
            "decoration.content_inset",
        ] {
            assert!(
                r.diffs.iter().any(|d| d.field == field),
                "missing diff {field}"
            );
        }
        let mut cur = old;
        let applied = reconcile_live(&mut cur, &new).expect("live must reconcile");
        assert_eq!(applied.overall, ReloadClass::Live);
        assert_eq!(cur.decoration, crate::types::DecorationConfig::safe());
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

    #[test]
    fn diff_selection_auto_copy_is_restart_required() {
        // CTX-0191: flipping auto-copy must surface as a restart-required
        // diff (no silent no-op on reload).
        let old = EffectiveConfig::default();
        assert!(old.selection.auto_copy);
        let mut new = old.clone();
        new.selection.auto_copy = false;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "selection.auto_copy"));
    }

    #[test]
    fn diff_scrollbar_is_restart_required() {
        // CTX-0181: changing mode or width must surface as a
        // restart-required diff (chrome is adopted at startup).
        use crate::types::ScrollbarMode;
        let old = EffectiveConfig::default();
        assert_eq!(old.scrollbar.mode, ScrollbarMode::Hidden);
        let mut new = old.clone();
        new.scrollbar.mode = ScrollbarMode::Auto;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "scrollbar.mode"));
        let mut new2 = old.clone();
        new2.scrollbar.width = 12;
        let r2 = diff(&old, &new2);
        assert_eq!(r2.overall, ReloadClass::RestartRequired);
        assert!(r2.diffs.iter().any(|d| d.field == "scrollbar.width"));
    }

    #[test]
    fn diff_mouse_focus_follows_mouse_is_restart_required() {
        // CTX-0260: flipping hover-focus must surface as a restart-required
        // diff (chrome is adopted at startup into RuntimeConfig).
        let old = EffectiveConfig::default();
        assert!(!old.mouse.focus_follows_mouse);
        assert_eq!(
            classify_field("mouse.focus_follows_mouse"),
            ReloadClass::RestartRequired
        );
        assert_eq!(classify_field("mouse"), ReloadClass::RestartRequired);
        let mut new = old.clone();
        new.mouse.focus_follows_mouse = true;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(
            r.diffs
                .iter()
                .any(|d| d.field == "mouse.focus_follows_mouse")
        );
    }

    #[test]
    fn diff_layout_gaps_is_restart_required() {
        // CTX-0177: changing either gap must surface as a restart-required
        // diff (no silent no-op on reload).
        let old = EffectiveConfig::default();
        assert_eq!(old.layout.gaps_in, 0);
        let mut new = old.clone();
        new.layout.gaps_in = 2;
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "layout.gaps_in"));
        let mut new2 = old.clone();
        new2.layout.gaps_out = 1;
        let r2 = diff(&old, &new2);
        assert_eq!(r2.overall, ReloadClass::RestartRequired);
        assert!(r2.diffs.iter().any(|d| d.field == "layout.gaps_out"));
    }

    #[test]
    fn diff_theme_and_keymaps_are_live_and_reconcile() {
        // CTX-0295: `appearance.theme` and `keymaps` are declared Live but
        // had no diff/reconcile test (only the class lookup in
        // `classify_table`). A live reload must list both and apply them.
        use crate::types::KeymapEntry;
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.appearance.theme = Some("dark".into());
        new.keymaps = vec![KeymapEntry {
            chord: "alt+h".into(),
            action: "focus_next".into(),
            context: "global".into(),
        }];
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::Live);
        assert!(!r.needs_restart);
        assert!(!r.has_rejected);
        assert!(r.diffs.iter().any(|d| d.field == "appearance.theme"));
        assert!(r.diffs.iter().any(|d| d.field == "keymaps"));
        let mut cur = old;
        reconcile_live(&mut cur, &new).expect("theme + keymaps must reconcile live");
        assert_eq!(cur.appearance.theme.as_deref(), Some("dark"));
        assert_eq!(cur.keymaps.len(), 1);
    }

    #[test]
    fn diff_terminal_shell_is_restart_required() {
        // CTX-0295: `terminal.shell` is spawn-time state; a change must be
        // reported restart-required (never silently live) and must be
        // refused by `reconcile_live`.
        let old = EffectiveConfig::default();
        let mut new = old.clone();
        new.terminal.shell = Some("/bin/fish".into());
        let r = diff(&old, &new);
        assert_eq!(r.overall, ReloadClass::RestartRequired);
        assert!(r.needs_restart);
        assert!(r.diffs.iter().any(|d| d.field == "terminal.shell"));
        let mut cur = old;
        assert!(reconcile_live(&mut cur, &new).is_err());
        assert_eq!(cur.terminal.shell, None, "previous value stays active");
    }
}
