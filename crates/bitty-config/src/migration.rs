//! Versioned migration for the terminal configuration shape.
//!
//! `init.lua` returns a plain-data table. [`CURRENT_CONFIG_VERSION`] names the
//! shape this crate understands; a file that omits the top-level
//! `config_version` key is version `0` (the pre-version shape). The typed
//! plan carries that number in [`ConfigPlan::schema_version`] so transforms
//! and validation share one carrier.
//!
//! Transforms are explicit, named data transforms from one source version to
//! the next, registered in [`CONFIG_TRANSFORMS`]. There is no guessing: a
//! declared version newer than [`CURRENT_CONFIG_VERSION`] fails closed with
//! [`ConfigError::ConfigVersionUnsupported`], and a gap in the transform table
//! fails closed with [`ConfigError::ConfigMigrationFailed`].
//!
//! # Adding version 2
//!
//! 1. Write one named transform `fn migrate_config_v1_to_v2(ConfigPlan) ->
//!    Result<ConfigPlan, ConfigError>` and register it in [`CONFIG_TRANSFORMS`].
//! 2. Add one test that migrates a version-1 fixture and asserts the renamed
//!    or reshaped key.
//! 3. Add one entry to the config guide describing the change and the new
//!    transform.
//!
//! Bump [`CURRENT_CONFIG_VERSION`] in the same change. `CURRENT_SCHEMA_VERSION`
//! is the historical alias for the same value.

use crate::error::ConfigError;
use crate::plan::ConfigPlan;

/// Current terminal configuration shape understood by this crate.
///
/// Version `1` is the first explicit shape: the same keys as the pre-version
/// (version `0`) shape plus the top-level `config_version = 1` stamp. Version
/// `0` is accepted at startup exactly as before; it migrates to `1` only
/// through an explicit [`migrate`] call (for example `bitty config migrate`).
///
/// Files declaring a higher version fail closed
/// ([`ConfigError::ConfigVersionUnsupported`]); this crate never guesses a
/// future shape.
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// Historical name for [`CURRENT_CONFIG_VERSION`] (the typed plan API used
/// `schema_version` before the Lua `config_version` key existed).
pub const CURRENT_SCHEMA_VERSION: u32 = CURRENT_CONFIG_VERSION;

/// One named transform from a source config version to the next.
pub type ConfigTransform = fn(ConfigPlan) -> Result<ConfigPlan, ConfigError>;

/// Transform table keyed by **source** version.
///
/// Each entry upgrades exactly one version. [`migrate`] applies entries in
/// ascending order until the plan reaches [`CURRENT_CONFIG_VERSION`]. A
/// missing source version is a hard failure, never a silent skip.
pub const CONFIG_TRANSFORMS: &[(u32, ConfigTransform)] = &[(0, migrate_config_v0_to_v1)];

/// Version `0` (absent `config_version`) -> version `1`.
///
/// The first shape only stamps the version: no key was renamed or removed
/// yet. The transform stays a named function so the next version has a slot
/// and so the change is reviewable.
///
/// # Errors
///
/// Never fails today; the signature is the table contract for future
/// transforms that can reject an untranslatable shape.
pub fn migrate_config_v0_to_v1(mut plan: ConfigPlan) -> Result<ConfigPlan, ConfigError> {
    plan.schema_version = Some(1);
    Ok(plan)
}

/// Migrate a plan to [`CURRENT_CONFIG_VERSION`] if needed.
///
/// - `None` / `0` -> `1`: [`migrate_config_v0_to_v1`].
/// - `1` -> `1`: already current, returned unchanged.
/// - `> 1`: unsupported, returns [`ConfigError::ConfigVersionUnsupported`].
///
/// Validation of the plan is performed after migration.
///
/// # Errors
///
/// [`ConfigError::ConfigVersionUnsupported`] for a newer declared version,
/// [`ConfigError::ConfigMigrationFailed`] when no transform covers a source
/// version, and whatever a transform or [`ConfigPlan::validate`] reports.
pub fn migrate(mut plan: ConfigPlan) -> Result<ConfigPlan, ConfigError> {
    let from = plan.effective_schema_version();
    if from > CURRENT_CONFIG_VERSION {
        return Err(ConfigError::ConfigVersionUnsupported {
            found: from,
            supported: CURRENT_CONFIG_VERSION,
        });
    }
    let mut version = from;
    while version < CURRENT_CONFIG_VERSION {
        let transform = CONFIG_TRANSFORMS
            .iter()
            .find(|(source, _)| *source == version)
            .map(|(_, transform)| *transform)
            .ok_or_else(|| ConfigError::ConfigMigrationFailed {
                from: version,
                to: CURRENT_CONFIG_VERSION,
                message: format!("no transform registered for config_version {version}"),
            })?;
        plan = transform(plan)?;
        version += 1;
    }
    plan.validate()?;
    Ok(plan)
}

/// Describe whether a plan needs migration.
#[must_use]
pub fn needs_migration(plan: &ConfigPlan) -> bool {
    plan.effective_schema_version() != CURRENT_CONFIG_VERSION
}

/// One key-level change in a migration (`old -> new`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    /// Dotted key path (`config_version`, `font.family`, ...).
    pub key: String,
    /// Previous value; `None` means the key was absent.
    pub from: Option<String>,
    /// New value; `None` means the key is removed.
    pub to: Option<String>,
}

impl ConfigChange {
    /// Human row: `key = (old|absent) -> (new|removed)`.
    #[must_use]
    pub fn render(&self) -> String {
        let from = self.from.as_deref().unwrap_or("(absent)");
        let to = self.to.as_deref().unwrap_or("(removed)");
        format!("{} = {from} -> {to}", self.key)
    }
}

/// Change list for a plan at its declared version.
///
/// Each transform is responsible for appending its own entries here when it
/// renames or reshapes keys; the version stamp is always included. A plan
/// already at [`CURRENT_CONFIG_VERSION`] yields an empty list.
#[must_use]
pub fn config_changes(plan: &ConfigPlan) -> Vec<ConfigChange> {
    let from = plan.effective_schema_version();
    if from >= CURRENT_CONFIG_VERSION {
        return Vec::new();
    }
    vec![ConfigChange {
        key: String::from("config_version"),
        from: plan.schema_version.map(|version| version.to_string()),
        to: Some(CURRENT_CONFIG_VERSION.to_string()),
    }]
}

/// Rewrites `config_version` in a Lua config source, preserving every other
/// byte (comments, formatting, and user values).
///
/// The returned table literal is located at the last top-level `return`.
/// An existing top-level `config_version = <integer>` value is replaced;
/// otherwise the key is inserted as the first field of the returned table.
///
/// # Errors
///
/// [`ConfigError::ConfigMigrationFailed`] when the returned value is not a
/// table literal or an existing `config_version` value is not an integer
/// literal. Callers validate the rewritten source before writing it.
pub fn stamp_config_version(source: &str, version: u32) -> Result<String, ConfigError> {
    let table_open =
        returned_table_open(source).ok_or_else(|| ConfigError::ConfigMigrationFailed {
            from: 0,
            to: version,
            message: String::from(
                "cannot locate the returned table literal; add `config_version = <n>` manually",
            ),
        })?;
    match config_version_slot(source, table_open) {
        ConfigVersionSlot::Literal(range) => {
            let mut out = String::with_capacity(source.len());
            out.push_str(&source[..range.start]);
            out.push_str(&version.to_string());
            out.push_str(&source[range.end..]);
            return Ok(out);
        }
        ConfigVersionSlot::NonLiteral => {
            return Err(ConfigError::ConfigMigrationFailed {
                from: 0,
                to: version,
                message: String::from(
                    "the existing `config_version` value is not an integer literal; set it manually",
                ),
            });
        }
        ConfigVersionSlot::Absent => {}
    }
    let after = source[table_open + 1..].chars().next();
    let insertion = match after {
        Some('\n') => format!(
            "\n{}config_version = {version},",
            table_indent(source, table_open)
        ),
        Some('\r') => format!(
            "\r\n{}config_version = {version},",
            table_indent(source, table_open)
        ),
        _ => {
            let rest = source[table_open + 1..].trim_start();
            if rest.starts_with('}') {
                format!(" config_version = {version} ")
            } else {
                format!(" config_version = {version},")
            }
        }
    };
    let mut out = String::with_capacity(source.len() + insertion.len());
    out.push_str(&source[..=table_open]);
    out.push_str(&insertion);
    out.push_str(&source[table_open + 1..]);
    Ok(out)
}

/// Byte offset of the `{` that opens the table returned by the chunk.
///
/// Scans Lua tokens (strings, long brackets, line/block comments) and keeps
/// the last `return` seen outside any table/paren nesting, then requires the
/// next non-space byte to be `{`.
fn returned_table_open(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut table_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut last_return: Option<usize> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index = skip_comment(source, index);
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            index = skip_quoted(source, index, byte);
            continue;
        }
        if byte == b'[' {
            if let Some(end) = skip_long_bracket(source, index) {
                index = end;
                continue;
            }
        }
        match byte {
            b'{' => table_depth += 1,
            b'}' => table_depth = table_depth.saturating_sub(1),
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            _ => {}
        }
        if table_depth == 0 && paren_depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            if &source[start..index] == "return" {
                last_return = Some(index);
            }
            continue;
        }
        index += 1;
    }
    let mut cursor = last_return?;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'{') {
        Some(cursor)
    } else {
        None
    }
}

/// Location of the returned table's top-level `config_version` slot.
enum ConfigVersionSlot {
    /// No top-level `config_version` key.
    Absent,
    /// Byte range of an integer literal value.
    Literal(std::ops::Range<usize>),
    /// The key exists but its value is not an integer literal.
    NonLiteral,
}

/// Finds the top-level `config_version` slot inside the returned table.
fn config_version_slot(source: &str, table_open: usize) -> ConfigVersionSlot {
    let bytes = source.as_bytes();
    let mut index = table_open + 1;
    let mut depth = 1usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index = skip_comment(source, index);
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            index = skip_quoted(source, index, byte);
            continue;
        }
        if byte == b'[' {
            if let Some(end) = skip_long_bracket(source, index) {
                index = end;
                continue;
            }
        }
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return ConfigVersionSlot::Absent;
                }
            }
            _ => {}
        }
        if depth == 1 && (byte.is_ascii_alphabetic() || byte == b'_') {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            if &source[start..index] != "config_version" {
                continue;
            }
            let mut cursor = index;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'=') {
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let value_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor > value_start {
                return ConfigVersionSlot::Literal(value_start..cursor);
            }
            return ConfigVersionSlot::NonLiteral;
        }
        index += 1;
    }
    ConfigVersionSlot::Absent
}

/// Indentation of the first content line inside the returned table.
fn table_indent(source: &str, table_open: usize) -> String {
    let rest = &source[table_open + 1..];
    for line in rest.lines().skip(1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent_len = line.len() - trimmed.len();
        return line[..indent_len].to_string();
    }
    String::from("    ")
}

/// Skips a `--` line comment or a `--[[ ... ]]` block comment.
fn skip_comment(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    if bytes.get(start + 2) == Some(&b'[') {
        if let Some(end) = skip_long_bracket(source, start + 2) {
            return end;
        }
    }
    let mut index = start + 2;
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

/// Skips a quoted string starting at `start` (the opening quote byte).
fn skip_quoted(source: &str, start: usize, quote: u8) -> usize {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    index
}

/// Skips a Lua long bracket (`[[...]]`, `[=[...]=]`, ...) at `start`.
fn skip_long_bracket(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut equals = 0usize;
    let mut cursor = start + 1;
    while bytes.get(cursor) == Some(&b'=') {
        equals += 1;
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }
    cursor += 1;
    let mut index = cursor;
    while index < bytes.len() {
        if bytes[index] == b']' {
            let mut close = index + 1;
            let mut seen = 0usize;
            while seen < equals && bytes.get(close) == Some(&b'=') {
                seen += 1;
                close += 1;
            }
            if seen == equals && bytes.get(close) == Some(&b']') {
                return Some(close + 1);
            }
        }
        index += 1;
    }
    Some(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ConfigPlan;
    use crate::types::FontConfig;

    #[test]
    fn current_does_not_migrate() {
        let plan = ConfigPlan {
            schema_version: Some(CURRENT_CONFIG_VERSION),
            ..Default::default()
        };
        let out = migrate(plan.clone()).expect("current must migrate trivially");
        assert_eq!(out.effective_schema_version(), CURRENT_CONFIG_VERSION);
        assert!(config_changes(&plan).is_empty());
    }

    #[test]
    fn zero_migrates_to_current() {
        let plan = ConfigPlan {
            schema_version: Some(0),
            font: Some(FontConfig {
                family: "Mono".into(),
                size: 12.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = migrate(plan).expect("0->1 must succeed");
        assert_eq!(out.effective_schema_version(), CURRENT_CONFIG_VERSION);
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
        assert_eq!(out.effective_schema_version(), CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn future_version_rejected_naming_the_version() {
        let plan = ConfigPlan {
            schema_version: Some(2),
            ..Default::default()
        };
        match migrate(plan) {
            Err(ConfigError::ConfigVersionUnsupported { found, supported }) => {
                assert_eq!(found, 2);
                assert_eq!(supported, CURRENT_CONFIG_VERSION);
            }
            other => panic!("expected ConfigVersionUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn needs_migration_false_for_current() {
        let plan = ConfigPlan {
            schema_version: Some(CURRENT_CONFIG_VERSION),
            ..Default::default()
        };
        assert!(!needs_migration(&plan));
    }

    #[test]
    fn changes_name_the_version_and_absence() {
        let plan = ConfigPlan {
            schema_version: None,
            ..Default::default()
        };
        let changes = config_changes(&plan);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key, "config_version");
        assert_eq!(changes[0].from, None);
        assert_eq!(changes[0].to.as_deref(), Some("1"));
        assert!(changes[0].render().contains("(absent) -> 1"));
    }

    #[test]
    fn transform_table_covers_zero_to_one() {
        assert_eq!(CONFIG_TRANSFORMS.len(), 1);
        assert_eq!(CONFIG_TRANSFORMS[0].0, 0);
    }

    #[test]
    fn stamp_inserts_into_multiline_table() {
        let source = "-- keep me\nreturn {\n    font = { family = \"Mono\" },\n}\n";
        let out = stamp_config_version(source, 1).expect("stamp");
        assert!(out.contains("-- keep me"));
        assert!(out.contains("    config_version = 1,"));
        assert!(out.contains("font = { family = \"Mono\" }"));
    }

    #[test]
    fn stamp_inserts_into_single_line_table() {
        let out =
            stamp_config_version("return { font = { family = \"Mono\" } }\n", 1).expect("stamp");
        assert!(out.contains("config_version = 1,"));
        assert!(out.contains("font = { family = \"Mono\" }"));
    }

    #[test]
    fn stamp_replaces_existing_zero() {
        let source = "return {\n    config_version = 0,\n    font = {},\n}\n";
        let out = stamp_config_version(source, 1).expect("stamp");
        assert!(out.contains("config_version = 1,"));
        assert!(!out.contains("config_version = 0"));
    }

    #[test]
    fn stamp_refuses_a_non_literal_version() {
        let source = "return {\n    config_version = VERSION,\n}\n";
        let error = stamp_config_version(source, 1).expect_err("non-literal must refuse");
        assert!(
            error.to_string().contains("not an integer literal"),
            "{error}"
        );
    }

    #[test]
    fn stamp_refuses_a_non_table_return() {
        assert!(stamp_config_version("return 42\n", 1).is_err());
    }

    #[test]
    fn stamp_ignores_nested_return_and_comments() {
        let source = concat!(
            "-- return { not_me = true }\n",
            "local function helper()\n",
            "    return { nested = true }\n",
            "end\n",
            "return {\n",
            "    font = { family = \"Mono\" },\n",
            "}\n",
        );
        let out = stamp_config_version(source, 1).expect("stamp");
        assert!(out.contains("-- return { not_me = true }"));
        assert!(out.contains("return { nested = true }"));
        assert!(out.contains("    config_version = 1,"));
    }
}
