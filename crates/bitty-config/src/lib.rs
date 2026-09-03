//! `bitty-config`: draft typed configuration pipeline for Bitty.
//!
//! # Draft status — experimental implementation as review evidence
//!
//! This crate implements the **proposed** contracts from
//! `bitty-docs/docs/specifications/configuration-model-rfc.md`
//! (`Proposed` / `draft`, `OQ-010`). That RFC remains `Proposed` (not
//! `Accepted`/`normative`) until independent review (category owner + docs
//! curator + security reviewer) accepts it and an ADR records acceptance.
//! Per the new RFC lifecycle (`Draft -> experimental review evidence ->
//! Accepted -> normative` per `bitty-docs/docs/specifications/README.md` and
//! `docs/development/documentation-workflow.md`), this crate's **experimental
//! implementation of Candidate A** serves as review evidence and carries no
//! compatibility promise; do not describe its behavior as shipped/stable until
//! the RFC is `Accepted` and a release ships it.
//!
//! **Candidate A accepted for v1:** the two-stage declarative plan
//! `Lua -> ConfigPlan -> typed validation -> merge -> diff -> reconcile` is
//! the accepted configuration pipeline for v1 (see
//! `configuration-model-rfc.md` § “Candidate A: two-stage declarative plan with
//! Rust reconciliation”). Candidate C (hybrid declarative plus bounded
//! imperative overlay) is **deferred** to a future
//! `plugin/runtime overlay` RFC and is not part of the v1 contract.
//!
//! The RFC's Lua Runtime dependency (`lua-runtime-rfc`, `OQ-009`) is also
//! still proposed, so this crate is **pure data + validation**: it takes an
//! already evaluated [`ConfigPlan`] as plain data and owns everything after
//! it. There is no Lua VM coupling, no file I/O, no platform path
//! resolution, and no `unsafe` — the crate is headlessly testable on both
//! Linux CI and the `windows-latest` job.
//!
//! # Pipeline — Candidate A accepted for v1 (experimental review evidence)
//!
//! ```text
//! Lua (outside crate) -> ConfigPlan -> typed validation -> migration -> merge -> diff -> reconcile
//! ```
//!
//! - Lua evaluation is side-effect-free by construction and outside this
//!   crate. The host evaluates modules to a [`plan::ConfigPlan`] value.
//! - Typed validation (`validation`, `types`) checks every present field.
//! - Migration (`migration`) bumps older `schema_version`s to the current
//!   version via stub transforms.
//! - Merge (`merge`) sorts by [`plan::LayerKind::precedence`], deep-merges
//!   structured maps, merges sets by identifier, replaces scalars, reports
//!   conflicts with source attribution, and rejects non-overridable policy.
//! - Reload diff (`reload`) classifies every schema change as
//!   [`reload::ReloadClass::Live`] / [`reload::ReloadClass::RestartRequired`]
//!   / [`reload::ReloadClass::Rejected`] — declared by the schema, never
//!   inferred — and offers [`reload::reconcile_live`] for the last-good-plan
//!   retention that `R-009` needs.
//! - Project trust (`trust`) implements the T-08 hash-bound consent lifecycle
//!   over declarative data only (no project-scope Lua execution).
//! - Failure recovery is `fallback_builtin` (`bitty --safe` minimal config) or
//!   retaining the last good plan — both headless helpers.
//!
//! # RFC section mapping
//!
//! | RFC section | Module(s) | Key items |
//! |-------------|-----------|-----------|
//! | Pipeline candidates (§Candidate A, accepted for v1) | `plan`, `validation`, `migration`, `merge`, `reload` | [`plan::ConfigPlan`] plain data; `ConfigPlan -> validation -> merge -> diff -> reconcile` in safe Rust, no half-mutated terminal state; Candidate A accepted for v1 as experimental review evidence per new RFC lifecycle (`Draft -> experimental review evidence -> Accepted -> normative`), Candidate C deferred to plugin/runtime overlay RFC |
//! | Layers, merge, and attribution | `plan`, `merge` | [`plan::LayerKind`] 8-layer stack + precedence; [`merge::MergeClass`] per-field table; [`merge::MergeConflict`] reporting; attribution `HashMap<String, ConfigSource>` surviving merge for `config show --source` |
//! | System policy non-overridable | `merge`, `error` | [`plan::LayerKind::SystemPolicy`] + [`error::ConfigError::NonOverridable`] distinct from system defaults |
//! | Profile `extends` | `merge` | [`merge::resolve_profile_chain`] single-parent chains with cycle detection; multiple inheritance remains an open item |
//! | Reload classification | `reload` | [`reload::ReloadClass`], [`reload::classify_field`], [`reload::diff`], [`reload::reconcile_live`] — classification declared by schema, reload reuses startup validation/merge, restart-required reported upfront |
//! | Project trust | `trust` | Declarative-only; path+hash consent binding; [`trust::TrustStore`], [`trust::validate_project_plan`]; trust DB location/expiry/rename/UX remain open |
//! | Failure and safe-mode interaction | `reload` | [`reload::fallback_builtin`], [`reload::should_retain_previous`], last-good-plan retention |
//! | Security review notes | `trust`, `merge`, `reload`, `error` | No control downgrades normative baseline; overlay writes are rejected at validation (no privileged project fields) — negative tests included |
//! | Open items remaining under OQ-010 | doc only | Documented honestly: A vs C decision resolved (Candidate A accepted for v1 as experimental review evidence, Candidate C deferred to plugin/runtime overlay RFC), schema-sync tooling, reload table stability, trust DB, multi-parent `extends`, manifest/lock coexistence, native path mappings — see `configuration-model-rfc.md` § Open items remaining under OQ-010 (resolved vs migrated) |
//!
//! # Ownership rules
//!
//! - No workspace-crate dependencies.
//! - No third-party dependencies (pure `std`).
//! - `#![forbid(unsafe_code)]` at crate and workspace level; `safe` mode and
//!   bounded limits make the exception unnecessary.
//! - `MSRV 1.85`, `edition = "2024"`.
//! - All structures are owned (`String`, `Vec`, …), never `&str` — so plans,
//!   merged configs, diagnostics, and trust records are cloneable, comparable,
//!   and sendable without lifetimes.

#![forbid(unsafe_code)]

pub mod error;
pub mod merge;
pub mod migration;
pub mod plan;
pub mod reload;
pub mod theme;
pub mod trust;
pub mod types;
pub mod validation;

pub use error::{ConfigError, ErrorClass};
pub use merge::{
    MergeClass, MergeConflict, MergedConfig, merge_class_for, merge_layers, resolve_profile_chain,
    try_merge_layers,
};
pub use migration::{CURRENT_SCHEMA_VERSION, migrate, needs_migration};
pub use plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
pub use reload::{
    ReloadClass, ReloadReport, classify_field, diff, fallback_builtin, reconcile_live,
};
pub use theme::{
    BITTY_DARK, DARK_THEME_ALIAS, DEFAULT_THEME_NAME, Theme, ThemeResolution, default_theme,
    normalize_theme_name, resolve_theme, resolve_theme_with_status,
};
pub use trust::{TrustDecision, TrustRecord, TrustStore, check_trust, validate_project_plan};
pub use types::{
    AppearanceConfig, EffectiveConfig, FontConfig, KeymapEntry, PluginSpec, TerminalConfig,
    WindowConfig,
};
pub use validation::{Validate, collect_diagnostics, validate_stack};

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::plan::{ConfigPlan, ConfigSource, LayerKind, LayeredPlan};
    use crate::types::{AppearanceConfig, FontConfig, TerminalConfig, WindowConfig};

    #[test]
    fn end_to_end_pipeline() {
        // Simulate Lua producing two layers.
        let system = LayeredPlan::new(
            ConfigSource::new(
                LayerKind::SystemDefaults,
                Some("/etc/xdg/bitty/defaults.lua"),
            ),
            ConfigPlan {
                font: Some(FontConfig {
                    family: "JetBrains Mono".into(),
                    size: 12.0,
                }),
                window: Some(WindowConfig {
                    opacity: 0.95,
                    padding: 8,
                }),
                schema_version: Some(CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );
        let user = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("/home/alice/.config/bitty/init.lua")),
            ConfigPlan {
                font: Some(FontConfig {
                    family: "Maple Mono".into(),
                    size: 14.0,
                }),
                terminal: Some(TerminalConfig {
                    scrollback: 50_000,
                    shell: None,
                }),
                appearance: Some(AppearanceConfig {
                    theme: Some("dark".into()),
                }),
                schema_version: Some(CURRENT_SCHEMA_VERSION),
                ..Default::default()
            },
        );

        validate_stack(&[system.clone(), user.clone()]).expect("stack valid");
        let migrated: Vec<LayeredPlan> = [system, user]
            .into_iter()
            .map(|mut lp| {
                lp.plan = migrate(lp.plan).expect("migration");
                lp
            })
            .collect();

        let merged = merge_layers(migrated).expect("merge");
        merged.effective.validate().expect("effective valid");

        // User overrides system font.
        assert_eq!(merged.effective.font.family, "Maple Mono");
        assert_eq!(merged.effective.font.size, 14.0);
        // User added terminal scrollback.
        assert_eq!(merged.effective.terminal.scrollback, 50_000);
        // Attribution answers source.
        assert_eq!(
            merged.source_of("font.family").unwrap().layer,
            LayerKind::User
        );
        // System window opacity retained when user didn't override window.
        // Actually user didn't set window, so system value survives.
        assert_eq!(merged.effective.window.opacity, 0.95);

        // Reload diff against defaults is restart-required because of scrollback.
        let defaults = EffectiveConfig::default();
        let report = diff(&defaults, &merged.effective);
        assert!(report.needs_restart);
        assert_eq!(report.overall, ReloadClass::RestartRequired);

        // Live reconcile must reject restart-required changes.
        let mut cur = defaults;
        assert!(reconcile_live(&mut cur, &merged.effective).is_err());
    }

    #[test]
    fn fallback_and_retain_previous() {
        let good = EffectiveConfig::default();
        let mut bad = good.clone();
        bad.font.size = f32::NAN;
        let r = diff(&good, &bad);
        assert!(r.has_rejected);
        assert!(crate::reload::should_retain_previous(&r));
        // Safe-mode fallback is always valid.
        fallback_builtin().validate().expect("fallback valid");
    }

    #[test]
    fn project_trust_end_to_end() {
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
            }),
            ..Default::default()
        };
        validate_project_plan(&plan).expect("allowed project fields");
        let mut store = TrustStore::new();
        store.insert(TrustRecord::new("/proj", "hash1", TrustDecision::TrustOnce));
        check_trust(&store, "/proj", "hash1").expect("trusted");
        check_trust(&store, "/proj", "hash2").unwrap_err();
    }
}
