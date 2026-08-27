//! Migration stubs for the typed schema.
//!
//! The typed Rust schema and the documented Lua shape can drift; one of them
//! must be generated or cross-checked in CI (RFC open item "Schema ownership
//! tooling"). Until that tooling exists, migrations are explicit data
//! transforms from older `schema_version` values to [`CURRENT_SCHEMA_VERSION`].
//!
//! These stubs preserve headless testability: no file I/O, no Lua, no
//! branching on host state. Real value conversions will be added when a
//! second schema version exists; today every stub returns the input unchanged
//! except for bumping the version number, which is the minimal behavior that
//! keeps the pipeline total and testable.
//!
//! # Draft status
//!
//! Schema version `1` is the only understood version. Version `0` is the
//! implicit version for plans that omitted `schema_version` entirely (older
//! starter configs). Future versions must add a dedicated test and a
//! migration step here.

use crate::error::ConfigError;
use crate::plan::ConfigPlan;

/// Current schema version understood by this crate.
///
/// Plans declaring a higher version fail validation (`SchemaVersionUnsupported`).
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Migrate a plan to [`CURRENT_SCHEMA_VERSION`] if needed.
///
/// - `None` / `0` -> `1` : bump version, no field transform yet (stub).
/// - `1` -> `1` : already current, returned unchanged.
/// - `> 1` : unsupported, returns `SchemaVersionUnsupported`.
///
/// Validation of the plan is performed after migration.
pub fn migrate(mut plan: ConfigPlan) -> Result<ConfigPlan, ConfigError> {
    let from = plan.effective_schema_version();
    if from > CURRENT_SCHEMA_VERSION {
        return Err(ConfigError::SchemaVersionUnsupported {
            found: from,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if from == CURRENT_SCHEMA_VERSION {
        plan.validate()?;
        return Ok(plan);
    }
    // from == 0 -> 1 stub migration
    match from {
        0 => {
            // In a real migration this would rename or reshape fields,
            // fill newly required defaults, or translate deprecated shapes.
            // Today the typed schema has not yet diverged, so the stub
            // is a version bump with a validation pass.
            plan.schema_version = Some(CURRENT_SCHEMA_VERSION);
            plan.validate()?;
            Ok(plan)
        }
        _ => Err(ConfigError::MigrationFailed {
            from,
            to: CURRENT_SCHEMA_VERSION,
            message: format!("no migration path from {from}"),
        }),
    }
}

/// Describe whether a plan needs migration.
#[must_use]
pub fn needs_migration(plan: &ConfigPlan) -> bool {
    plan.effective_schema_version() != CURRENT_SCHEMA_VERSION
}

/// Stub for a future field rename migration (kept as a named function so
/// its presence is reviewable even before it is needed).
///
/// This is dead code today and is exposed for test/documentation purposes.
/// It does not yet alter the plan; when a rename lands, this will contain
/// the authoritative transform and a round-trip test.
#[must_use]
pub fn migrate_rename_field_stub(
    plan: ConfigPlan,
    _from_field: &str,
    _to_field: &str,
) -> ConfigPlan {
    // No-op stub: the field tables have not diverged yet.
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ConfigPlan;
    use crate::types::FontConfig;

    #[test]
    fn current_does_not_migrate() {
        let plan = ConfigPlan {
            schema_version: Some(CURRENT_SCHEMA_VERSION),
            ..Default::default()
        };
        let out = migrate(plan.clone()).expect("current must migrate trivially");
        assert_eq!(out.effective_schema_version(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn zero_migrates_to_current() {
        let plan = ConfigPlan {
            schema_version: Some(0),
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
            }),
            ..Default::default()
        };
        let out = migrate(plan).expect("0->1 stub must succeed");
        assert_eq!(out.effective_schema_version(), CURRENT_SCHEMA_VERSION);
        assert_eq!(out.font.unwrap().family, "Mono");
    }

    #[test]
    fn none_version_treated_as_zero_and_migrates() {
        let plan = ConfigPlan {
            schema_version: None,
            ..Default::default()
        };
        assert!(needs_migration(&plan));
        let out = migrate(plan).expect("None->1 must succeed");
        assert_eq!(out.effective_schema_version(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn future_version_rejected() {
        let plan = ConfigPlan {
            schema_version: Some(99),
            ..Default::default()
        };
        assert!(migrate(plan).is_err());
    }

    #[test]
    fn needs_migration_false_for_current() {
        let plan = ConfigPlan {
            schema_version: Some(CURRENT_SCHEMA_VERSION),
            ..Default::default()
        };
        assert!(!needs_migration(&plan));
    }

    #[test]
    fn rename_stub_is_identity_today() {
        let plan = ConfigPlan {
            schema_version: Some(CURRENT_SCHEMA_VERSION),
            ..Default::default()
        };
        let out = migrate_rename_field_stub(plan.clone(), "old", "new");
        assert_eq!(out, plan);
    }
}
