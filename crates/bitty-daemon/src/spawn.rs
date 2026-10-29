//! Bounded spawn-specification parsing for `session.create` (contract
//! `D-01`, `D-14`; bitty#2790).
//!
//! The spawn specification is the eight-field subset the accepted contract
//! fixes: `argv`, `env`, `cwd`, `cols`, `rows`, and `term` (bitty-ipc
//! `session.create`). It is parsed here, bounded here, and never logged,
//! audited, or echoed: the service layer hashes it into the idempotency
//! fingerprint and drops it after the bind.
//!
//! The whole-specification byte bound dominates the per-field bounds. A
//! request whose fields are individually valid but whose combined
//! specification exceeds `max_spawn_spec_bytes` is refused before any spawn,
//! and no partial environment or child is created.
//!
//! # Field rules
//!
//! | Field | Rule |
//! | --- | --- |
//! | `argv` | 1 to `max_spawn_argv_entries` entries, each at most `max_spawn_field_bytes`; no NUL |
//! | `env` | At most `max_spawn_env_entries` explicit `name=value` entries; the child's environment, not a patch |
//! | `cwd` | One bounded absolute path |
//! | `cols`/`rows` | Positive, within the terminal grid bound |
//! | `term` | Optional; the daemon supplies `xterm-256color` when absent |
//!
//! `argv`, `env`, and `cwd` are opaque spawn data. They are never interpreted,
//! recorded, or logged, and they are dropped from memory after the bind.

use std::path::PathBuf;

use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::limits::{LimitId, Limits};

/// Maximum terminal columns or rows accepted in a spawn specification or a
/// `session.resize`.
///
/// Mirrors the terminal grid bound (`bitty-term-state` `MAX_GRID_DIM = 1000`)
/// so a hosted worker can never be sized beyond what the content owner can
/// represent, and stays inside `u16` for `TIOCSWINSZ`.
pub const TERM_DIM_MAX: u16 = 1000;

/// Default initial columns when `cols` is absent.
pub const TERM_COLS_DEFAULT: u16 = 80;

/// Default initial rows when `rows` is absent.
pub const TERM_ROWS_DEFAULT: u16 = 24;

/// Default `TERM` value when `term` is absent.
pub const TERM_DEFAULT: &str = "xterm-256color";

/// Domain separator for the spawn-spec fingerprint digest.
const SPEC_DIGEST_DOMAIN: &[u8] = b"bittyd/spawn-spec/v1";

/// Why a spawn specification was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnSpecError {
    /// A missing, mistyped, empty, or unsafe field.
    Invalid,
    /// A count, per-field byte, or whole-spec byte bound.
    Limit {
        /// The enforced bound.
        limit: u64,
        /// The observed value.
        actual: u64,
    },
}

/// One validated, bounded spawn specification.
///
/// The value owns the caller bytes only until the spawn returns; the service
/// drops it after the bind and never stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    term: String,
}

impl SpawnSpec {
    /// Parse `session.create` parameters for a worker spawn.
    ///
    /// # Errors
    ///
    /// [`SpawnSpecError::Invalid`] for a malformed or unsafe field, and
    /// [`SpawnSpecError::Limit`] for a count or byte bound.
    pub fn parse(
        fields: Option<&Map<String, Value>>,
        limits: &Limits,
    ) -> Result<Self, SpawnSpecError> {
        let argv_limit =
            usize::try_from(limits.effective(LimitId::MaxSpawnArgvEntries)).unwrap_or(usize::MAX);
        let env_limit =
            usize::try_from(limits.effective(LimitId::MaxSpawnEnvEntries)).unwrap_or(usize::MAX);
        let field_limit =
            usize::try_from(limits.effective(LimitId::MaxSpawnFieldBytes)).unwrap_or(usize::MAX);
        let spec_limit =
            usize::try_from(limits.effective(LimitId::MaxSpawnSpecBytes)).unwrap_or(usize::MAX);

        let argv = string_array(fields, "argv")?.ok_or(SpawnSpecError::Invalid)?;
        if argv.is_empty() {
            return Err(SpawnSpecError::Invalid);
        }
        check_count(argv.len(), argv_limit)?;
        let mut total = 0usize;
        for value in &argv {
            check_field(value, field_limit)?;
            if value.is_empty() {
                return Err(SpawnSpecError::Invalid);
            }
            total = total.saturating_add(value.len());
        }

        let env = string_array(fields, "env")?.unwrap_or_default();
        check_count(env.len(), env_limit)?;
        let mut parsed_env = Vec::with_capacity(env.len());
        for entry in &env {
            check_field(entry, field_limit)?;
            let Some((name, value)) = entry.split_once('=') else {
                return Err(SpawnSpecError::Invalid);
            };
            if name.is_empty() || name.contains('\0') || value.contains('\0') {
                return Err(SpawnSpecError::Invalid);
            }
            total = total.saturating_add(entry.len());
            parsed_env.push((name.to_owned(), value.to_owned()));
        }

        let cwd = string_field(fields, "cwd")?.ok_or(SpawnSpecError::Invalid)?;
        check_field(cwd, field_limit)?;
        let cwd = PathBuf::from(cwd);
        if !cwd.is_absolute() {
            return Err(SpawnSpecError::Invalid);
        }
        total = total.saturating_add(cwd.as_os_str().len());

        let cols = dimension(fields, "cols")?.unwrap_or(TERM_COLS_DEFAULT);
        let rows = dimension(fields, "rows")?.unwrap_or(TERM_ROWS_DEFAULT);

        let term = string_field(fields, "term")?
            .map_or_else(|| TERM_DEFAULT.to_owned(), ToOwned::to_owned);
        check_field(&term, field_limit)?;
        if term.is_empty() {
            return Err(SpawnSpecError::Invalid);
        }
        total = total.saturating_add(term.len());

        if total > spec_limit {
            return Err(SpawnSpecError::Limit {
                limit: u64::try_from(spec_limit).unwrap_or(u64::MAX),
                actual: u64::try_from(total).unwrap_or(u64::MAX),
            });
        }

        Ok(Self {
            argv,
            env: parsed_env,
            cwd,
            cols,
            rows,
            term,
        })
    }

    /// The program and its arguments, `argv[0]` first.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The explicit child environment; never a patch over the daemon's.
    #[must_use]
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The absolute working directory.
    #[must_use]
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Initial columns.
    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// Initial rows.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// The `TERM` value for the child.
    #[must_use]
    pub fn term(&self) -> &str {
        &self.term
    }

    /// A domain-separated digest of the whole specification.
    ///
    /// The digest is the only form the idempotency store keeps: the raw argv,
    /// environment, and cwd never enter a descriptor, record, or audit line.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(SPEC_DIGEST_DOMAIN);
        for value in &self.argv {
            hash_field(&mut hasher, value.as_bytes());
        }
        hasher.update([0xff]);
        for (name, value) in &self.env {
            hash_field(&mut hasher, name.as_bytes());
            hash_field(&mut hasher, value.as_bytes());
        }
        hasher.update([0xff]);
        hash_field(&mut hasher, self.cwd.as_os_str().as_encoded_bytes());
        hasher.update(self.cols.to_be_bytes());
        hasher.update(self.rows.to_be_bytes());
        hash_field(&mut hasher, self.term.as_bytes());
        let digest = hasher.finalize();
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Validate one `session.resize` dimension.
///
/// # Errors
///
/// [`SpawnSpecError::Invalid`] for zero or an out-of-range value.
pub fn resize_dimension(value: u64) -> Result<u16, SpawnSpecError> {
    if value == 0 || value > u64::from(TERM_DIM_MAX) {
        return Err(SpawnSpecError::Invalid);
    }
    u16::try_from(value).map_err(|_| SpawnSpecError::Invalid)
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(bytes);
}

fn check_count(actual: usize, limit: usize) -> Result<(), SpawnSpecError> {
    if actual > limit {
        return Err(SpawnSpecError::Limit {
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
            actual: u64::try_from(actual).unwrap_or(u64::MAX),
        });
    }
    Ok(())
}

fn check_field(value: &str, limit: usize) -> Result<(), SpawnSpecError> {
    if value.len() > limit {
        return Err(SpawnSpecError::Limit {
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
            actual: u64::try_from(value.len()).unwrap_or(u64::MAX),
        });
    }
    if value.contains('\0') {
        return Err(SpawnSpecError::Invalid);
    }
    Ok(())
}

fn string_array(
    fields: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<Vec<String>>, SpawnSpecError> {
    let Some(value) = fields.and_then(|fields| fields.get(key)) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(SpawnSpecError::Invalid);
    };
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(SpawnSpecError::Invalid);
        };
        values.push(text.to_owned());
    }
    Ok(Some(values))
}

fn string_field<'a>(
    fields: Option<&'a Map<String, Value>>,
    key: &str,
) -> Result<Option<&'a str>, SpawnSpecError> {
    let Some(value) = fields.and_then(|fields| fields.get(key)) else {
        return Ok(None);
    };
    value.as_str().map(Some).ok_or(SpawnSpecError::Invalid)
}

fn dimension(
    fields: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<u16>, SpawnSpecError> {
    let Some(value) = fields.and_then(|fields| fields.get(key)) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or(SpawnSpecError::Invalid)?;
    resize_dimension(value).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    fn spec(value: Value) -> Result<SpawnSpec, SpawnSpecError> {
        SpawnSpec::parse(Some(&fields(value)), &Limits::default())
    }

    #[test]
    fn a_minimal_spec_takes_the_documented_defaults() {
        let parsed = spec(json!({
            "argv": ["/bin/true"],
            "cwd": "/",
        }))
        .expect("valid");
        assert_eq!(parsed.argv(), ["/bin/true"]);
        assert!(parsed.env().is_empty());
        assert_eq!(parsed.cwd(), &PathBuf::from("/"));
        assert_eq!(parsed.cols(), TERM_COLS_DEFAULT);
        assert_eq!(parsed.rows(), TERM_ROWS_DEFAULT);
        assert_eq!(parsed.term(), TERM_DEFAULT);
    }

    #[test]
    fn missing_and_malformed_fields_are_refused() {
        for value in [
            json!({"cwd": "/"}),
            json!({"argv": [], "cwd": "/"}),
            json!({"argv": [1], "cwd": "/"}),
            json!({"argv": ["a"], "cwd": "relative"}),
            json!({"argv": ["a"], "cwd": "/", "env": ["no-equals"]}),
            json!({"argv": ["a"], "cwd": "/", "env": ["=value"]}),
            json!({"argv": ["a"], "cwd": "/", "cols": 0}),
            json!({"argv": ["a"], "cwd": "/", "rows": 1001}),
            json!({"argv": ["a\0b"], "cwd": "/"}),
            json!({"argv": ["a"], "cwd": "/", "term": ""}),
        ] {
            assert_eq!(spec(value.clone()), Err(SpawnSpecError::Invalid), "{value}");
        }
    }

    #[test]
    fn every_count_and_byte_bound_is_refused_with_its_limit() {
        let limits = Limits::default();
        let argv = vec![
            "a".to_owned();
            usize::try_from(limits.effective(LimitId::MaxSpawnArgvEntries),)
                .expect("bound")
                + 1
        ];
        assert!(matches!(
            SpawnSpec::parse(Some(&fields(json!({"argv": argv, "cwd": "/"}))), &limits),
            Err(SpawnSpecError::Limit { .. })
        ));

        let field = "a".repeat(
            usize::try_from(limits.effective(LimitId::MaxSpawnFieldBytes)).expect("bound") + 1,
        );
        assert!(matches!(
            spec(json!({"argv": [field], "cwd": "/"})),
            Err(SpawnSpecError::Limit { .. })
        ));
    }

    #[test]
    fn the_whole_spec_bound_dominates_the_per_field_bounds() {
        let limits = Limits::default();
        let field = usize::try_from(limits.effective(LimitId::MaxSpawnFieldBytes)).expect("bound");
        let total = usize::try_from(limits.effective(LimitId::MaxSpawnSpecBytes)).expect("bound");
        let entries = (total / field) + 2;
        let argv = vec!["a".repeat(field); entries];
        assert!(matches!(
            SpawnSpec::parse(Some(&fields(json!({"argv": argv, "cwd": "/"}))), &limits),
            Err(SpawnSpecError::Limit { .. })
        ));
    }

    #[test]
    fn a_different_specification_has_a_different_digest() {
        let one = spec(json!({"argv": ["/bin/sh", "-c", "true"], "cwd": "/"})).expect("valid");
        let two = spec(json!({"argv": ["/bin/sh", "-c", "false"], "cwd": "/"})).expect("valid");
        let three = spec(json!({"argv": ["/bin/sh", "-c", "true"], "cwd": "/", "env": ["A=B"]}))
            .expect("valid");
        assert_ne!(one.digest(), two.digest());
        assert_ne!(one.digest(), three.digest());
        assert_eq!(one.digest().len(), 32);
        assert!(!one.digest().contains("true"));
    }
}
