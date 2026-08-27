//! Validation entry points.
//!
//! The typed Rust schema is the single validation authority: every field
//! is checked eagerly after the Lua evaluation produces a candidate plan.
//! Validation is total, side-effect free, and headless — no file I/O, no
//! process state.

use crate::error::ConfigError;
use crate::plan::{ConfigPlan, LayeredPlan};
use crate::types::{EffectiveConfig, MAX_KEYMAPS, MAX_PLUGINS};

/// Trait for structures that can validate themselves.
pub trait Validate {
    /// Validate this value, returning the first error.
    fn validate(&self) -> Result<(), ConfigError>;
}

impl Validate for ConfigPlan {
    fn validate(&self) -> Result<(), ConfigError> {
        ConfigPlan::validate(self)
    }
}

impl Validate for EffectiveConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        EffectiveConfig::validate(self)
    }
}

/// Validate a whole layer stack before merging.
///
/// Checks:
/// - every layer's plan is individually valid,
/// - no duplicate layer kinds that are expected to be singular (e.g. `User`),
///   though duplicates across `Profile` and `Distribution` are allowed,
/// - schema versions are not unsupported.
///
/// This is the function that `bitty config check` would call offline.
pub fn validate_stack(layers: &[LayeredPlan]) -> Result<(), ConfigError> {
    // Cheap deterministic check: detect duplicate singular layers.
    // Singular: CoreDefaults, SystemDefaults, SystemPolicy, User, Cli.
    // Multiplicity allowed: Distribution, Profile, TrustedLocal.
    let mut seen_singular = std::collections::HashSet::new();
    for lp in layers {
        lp.plan.validate()?;
        let singular = matches!(
            lp.source.layer,
            crate::plan::LayerKind::CoreDefaults
                | crate::plan::LayerKind::SystemDefaults
                | crate::plan::LayerKind::SystemPolicy
                | crate::plan::LayerKind::User
                | crate::plan::LayerKind::Cli
        );
        if singular {
            let key = lp.source.layer;
            if !seen_singular.insert(key) {
                return Err(ConfigError::validation(
                    "layers",
                    format!("duplicate singular layer '{}'", key.label()),
                ));
            }
            // Also validate unknown layer kind? already exhaustive.
            let _ = MAX_KEYMAPS;
            let _ = MAX_PLUGINS;
        }
    }
    Ok(())
}

/// Validate a single plan and return collected diagnostics (all errors that
/// can be found without merging).
///
/// For the incremental diagnostics contract borrowed from the Lua Runtime RFC,
/// this collects as many independent field errors as possible rather than
/// stopping at the first.
pub fn collect_diagnostics(plan: &ConfigPlan) -> Vec<ConfigError> {
    let mut out = Vec::new();
    if let Some(v) = plan.schema_version {
        if v > crate::migration::CURRENT_SCHEMA_VERSION {
            out.push(ConfigError::SchemaVersionUnsupported {
                found: v,
                supported: crate::migration::CURRENT_SCHEMA_VERSION,
            });
        }
    }
    for f in &plan.undeclared_fields {
        out.push(ConfigError::UndeclaredField {
            field: f.clone(),
            source: None,
        });
    }
    // Per-field validation; collect rather than short-circuit so the
    // diagnostic batch is complete.
    if let Some(v) = &plan.font {
        if let Err(e) = v.validate() {
            out.push(e);
        }
    }
    if let Some(v) = &plan.window {
        if let Err(e) = v.validate() {
            out.push(e);
        }
    }
    if let Some(v) = &plan.terminal {
        if let Err(e) = v.validate() {
            out.push(e);
        }
    }
    if let Some(v) = &plan.appearance {
        if let Err(e) = v.validate() {
            out.push(e);
        }
    }
    if let Some(v) = &plan.keymaps {
        if v.len() > crate::types::MAX_KEYMAPS {
            out.push(ConfigError::validation(
                "keymaps",
                format!("must contain <= {} entries", crate::types::MAX_KEYMAPS),
            ));
        } else {
            for km in v {
                if let Err(e) = km.validate() {
                    out.push(e);
                }
            }
        }
    }
    if let Some(v) = &plan.plugins {
        if v.len() > crate::types::MAX_PLUGINS {
            out.push(ConfigError::validation(
                "plugins",
                format!("must contain <= {} entries", crate::types::MAX_PLUGINS),
            ));
        } else {
            for p in v {
                if let Err(e) = p.validate() {
                    out.push(e);
                }
            }
        }
    }
    if let Some(ext) = &plan.extends {
        if ext.trim().is_empty() {
            out.push(ConfigError::validation(
                "extends",
                "must be non-empty when present",
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ConfigSource, LayerKind};
    use crate::types::{FontConfig, WindowConfig};

    #[test]
    fn stack_rejects_duplicate_user() {
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("a.lua")),
            ConfigPlan::default(),
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::User, Some("b.lua")),
            ConfigPlan::default(),
        );
        assert!(validate_stack(&[a, b]).is_err());
    }

    #[test]
    fn stack_allows_multiple_profiles() {
        let a = LayeredPlan::new(
            ConfigSource::new(LayerKind::Profile, Some("coding.lua")),
            ConfigPlan::default(),
        );
        let b = LayeredPlan::new(
            ConfigSource::new(LayerKind::Profile, Some("minimal.lua")),
            ConfigPlan::default(),
        );
        validate_stack(&[a, b]).expect("profiles may appear multiple times");
    }

    #[test]
    fn collect_diagnostics_is_not_short_circuit() {
        let plan = ConfigPlan {
            font: Some(FontConfig {
                family: "".into(),
                size: f32::NAN,
            }),
            window: Some(WindowConfig {
                opacity: 2.0,
                padding: 0,
            }),
            undeclared_fields: vec!["oops".into()],
            ..Default::default()
        };
        let diags = collect_diagnostics(&plan);
        // at least 3 errors: undeclared, font family empty, window opacity oob
        assert!(diags.len() >= 3);
    }

    #[test]
    fn empty_stack_valid() {
        validate_stack(&[]).expect("empty stack valid");
    }
}
