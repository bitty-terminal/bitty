//! Per-plugin execution budget tightening (OQ-014 / RC-1, RC-2; bitty#2304).
//!
//! The accepted host ceilings stay compiled constants and are the only
//! authority over how much work a plugin VM may perform. This module lets a
//! manifest and the user's local budget file *tighten* those ceilings; no
//! layer can raise them:
//!
//! | Layer               | Source                                   |
//! |---------------------|------------------------------------------|
//! | host default        | [`BudgetCeilings::HOST`] (RC-1 / RC-2)   |
//! | manifest declaration| `bitty-plugin.toml` `[limits]`           |
//! | user global default | `$XDG_CONFIG_HOME/bitty/plugin-budgets.lua` `default` |
//! | user plugin override| `$XDG_CONFIG_HOME/bitty/plugin-budgets.lua` `plugins["owner.name"]` |
//!
//! Every present value is an upper bound and the effective value is the
//! minimum across the layers. That relation is monotone: a later layer can
//! only tighten, never widen, even when it declares a larger number than an
//! earlier layer. Values outside `1..=ceiling` fail closed with a typed
//! [`PluginError::Budget`] carrying the field path; nothing is clamped or
//! silently ignored.
//!
//! The user file is deliberate, bounded, data-only Lua:
//!
//! ```lua
//! return {
//!   default = { fuel_instructions = 8000000, wall_ms = 40 },
//!   plugins = {
//!     ["xuepoo.hello"] = { wall_ms = 10 },
//!   },
//! }
//! ```
//!
//! It is parsed without a Lua VM or host access (bytes/lines/keys/duplicate
//! checks, UTF-8 only). A malformed file fails closed: activation refuses the
//! generation with a typed budget error rather than silently running on host
//! defaults. `--safe` ignores both the manifest declaration and the user file
//! and always runs the host defaults, so the recovery path cannot be blocked
//! by a bad local file.
//!
//! The advisory warning threshold (`warning_ms`) never suspends anything. It
//! is capped at `wall_ms - 1` when a tighter wall makes the configured
//! threshold unusable; the cap is reported by [`EffectiveBudgets`].

use std::collections::BTreeMap;

use crate::error::PluginError;
use crate::manifest::PluginId;

/// Canonical user budget file name under the Bitty configuration root.
pub const BUDGET_FILE_NAME: &str = "plugin-budgets.lua";

/// Maximum budget file size in bytes (fail-closed).
pub const MAX_BUDGET_FILE_BYTES: usize = 16 * 1024;

/// Maximum budget file lines (fail-closed).
pub const MAX_BUDGET_FILE_LINES: usize = 256;

/// Maximum bytes per budget file line (fail-closed).
pub const MAX_BUDGET_FILE_LINE_BYTES: usize = 512;

/// Maximum per-plugin entries in the user budget file (fail-closed).
pub const MAX_BUDGET_PLUGINS: usize = 256;

/// Host RC-1 instruction ceiling: 10,000,000 VM instructions per VM.
///
/// Mirrors `bitty_lua::RC1_INSTRUCTION_BUDGET`; `bitty-runtime` pins the
/// parity in a test so the two crates cannot drift silently.
pub const HOST_FUEL_INSTRUCTIONS_CEILING: u64 = 10_000_000;

/// Host RC-1 wall-clock ceiling: 50 ms per callback.
pub const HOST_WALL_MS_CEILING: u64 = 50;

/// Host RC-1 advisory warning threshold: 8 ms.
pub const HOST_WARNING_MS_CEILING: u64 = 8;

/// Host RC-2 per-plugin accounted-memory ceiling: 32 MiB.
pub const HOST_MEMORY_BYTES_CEILING: usize = 32 * 1024 * 1024;

/// Compiled host budget ceilings. This is the maximum any layer may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetCeilings {
    /// RC-1 instruction ceiling.
    pub fuel_instructions: u64,
    /// RC-1 wall-clock ceiling in milliseconds.
    pub wall_ms: u64,
    /// RC-1 advisory warning threshold ceiling in milliseconds.
    pub warning_ms: u64,
    /// RC-2 accounted-memory ceiling in bytes.
    pub memory_bytes: usize,
}

impl BudgetCeilings {
    /// The compiled RC-1/RC-2 ceilings.
    pub const HOST: Self = Self {
        fuel_instructions: HOST_FUEL_INSTRUCTIONS_CEILING,
        wall_ms: HOST_WALL_MS_CEILING,
        warning_ms: HOST_WARNING_MS_CEILING,
        memory_bytes: HOST_MEMORY_BYTES_CEILING,
    };
}

impl Default for BudgetCeilings {
    fn default() -> Self {
        Self::HOST
    }
}

/// One layer's declared budget tightening. `None` means "this layer says
/// nothing"; a present value is an upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetValues {
    /// Instruction fuel upper bound.
    pub fuel_instructions: Option<u64>,
    /// Wall-clock upper bound in milliseconds.
    pub wall_ms: Option<u64>,
    /// Accounted-memory upper bound in bytes.
    pub memory_bytes: Option<usize>,
    /// Advisory warning threshold in milliseconds.
    pub warning_ms: Option<u64>,
}

impl BudgetValues {
    /// Whether this layer declares no budget.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fuel_instructions.is_none()
            && self.wall_ms.is_none()
            && self.memory_bytes.is_none()
            && self.warning_ms.is_none()
    }

    /// Fail-closed validation against `ceilings`.
    ///
    /// Every present value must be `1..=ceiling`. When both `wall_ms` and
    /// `warning_ms` are present, the advisory warning must be strictly below
    /// the wall bound. `field_prefix` is the manifest/user field path used in
    /// diagnostics (for example `limits` or
    /// `plugin-budgets.plugins["xuepoo.hello"]`).
    ///
    /// # Errors
    ///
    /// [`PluginError::Budget`] naming the offending field path.
    pub fn validate(
        &self,
        field_prefix: &str,
        ceilings: BudgetCeilings,
    ) -> Result<(), PluginError> {
        check_range_u64(
            field_prefix,
            "fuel_instructions",
            self.fuel_instructions,
            ceilings.fuel_instructions,
        )?;
        check_range_u64(field_prefix, "wall_ms", self.wall_ms, ceilings.wall_ms)?;
        check_range_u64(
            field_prefix,
            "warning_ms",
            self.warning_ms,
            ceilings.warning_ms,
        )?;
        if let Some(bytes) = self.memory_bytes {
            let field = format!("{field_prefix}.memory_bytes");
            if bytes == 0 {
                return Err(PluginError::Budget {
                    field,
                    message: "must be at least 1".to_string(),
                });
            }
            if bytes > ceilings.memory_bytes {
                return Err(PluginError::Budget {
                    field,
                    message: format!("must be at most the host ceiling {}", ceilings.memory_bytes),
                });
            }
        }
        if let (Some(wall), Some(warning)) = (self.wall_ms, self.warning_ms) {
            if warning >= wall {
                return Err(PluginError::Budget {
                    field: format!("{field_prefix}.warning_ms"),
                    message: format!("must be less than wall_ms ({wall})"),
                });
            }
        }
        Ok(())
    }
}

fn check_range_u64(
    field_prefix: &str,
    key: &str,
    value: Option<u64>,
    ceiling: u64,
) -> Result<(), PluginError> {
    let Some(value) = value else {
        return Ok(());
    };
    let field = format!("{field_prefix}.{key}");
    if value == 0 {
        return Err(PluginError::Budget {
            field,
            message: "must be at least 1".to_string(),
        });
    }
    if value > ceiling {
        return Err(PluginError::Budget {
            field,
            message: format!("must be at most the host ceiling {ceiling}"),
        });
    }
    Ok(())
}

/// Layer that determined one effective budget dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// The compiled host default.
    HostDefault,
    /// The manifest's `[limits]` declaration.
    Manifest,
    /// The user file's global `default` table.
    UserDefault,
    /// The user file's per-plugin table.
    UserPlugin,
}

impl BudgetSource {
    /// Stable spelling for diagnostics and `plugin config show`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostDefault => "host-default",
            Self::Manifest => "manifest",
            Self::UserDefault => "user-default",
            Self::UserPlugin => "user-plugin",
        }
    }
}

/// Source attribution for every dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSources {
    /// Instruction fuel source.
    pub fuel_instructions: BudgetSource,
    /// Wall-clock source.
    pub wall_ms: BudgetSource,
    /// Accounted-memory source.
    pub memory_bytes: BudgetSource,
    /// Advisory warning source.
    pub warning_ms: BudgetSource,
}

/// Resolved effective budgets for one plugin generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveBudgets {
    /// Effective RC-1 instruction fuel.
    pub fuel_instructions: u64,
    /// Effective RC-1 wall clock in milliseconds.
    pub wall_ms: u64,
    /// Effective RC-2 accounted-memory ceiling in bytes.
    pub memory_bytes: usize,
    /// Effective advisory warning threshold in milliseconds.
    pub warning_ms: u64,
    /// Source attribution per dimension.
    pub sources: BudgetSources,
    /// Whether `warning_ms` was capped below its declared/derived value
    /// because the effective wall bound is tighter.
    pub warning_capped_by_wall: bool,
}

impl EffectiveBudgets {
    /// The compiled host defaults (every dimension sourced from
    /// [`BudgetSource::HostDefault`]).
    #[must_use]
    pub fn host_defaults(ceilings: BudgetCeilings) -> Self {
        Self {
            fuel_instructions: ceilings.fuel_instructions,
            wall_ms: ceilings.wall_ms,
            memory_bytes: ceilings.memory_bytes,
            warning_ms: ceilings.warning_ms.min(ceilings.wall_ms.saturating_sub(1)),
            sources: BudgetSources {
                fuel_instructions: BudgetSource::HostDefault,
                wall_ms: BudgetSource::HostDefault,
                memory_bytes: BudgetSource::HostDefault,
                warning_ms: BudgetSource::HostDefault,
            },
            warning_capped_by_wall: ceilings.warning_ms >= ceilings.wall_ms,
        }
    }
}

/// One user budget file: a global default plus per-plugin overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginBudgetFile {
    /// Global default applied to every plugin (tightening only).
    pub default: BudgetValues,
    /// Per-plugin overrides keyed by validated plugin id.
    pub plugins: BTreeMap<String, BudgetValues>,
}

impl PluginBudgetFile {
    /// Per-plugin values for `id`, if declared.
    #[must_use]
    pub fn for_plugin(&self, id: &str) -> BudgetValues {
        self.plugins.get(id).copied().unwrap_or_default()
    }

    /// Validate every table in the file against `ceilings`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Budget`] naming the first offending field path.
    pub fn validate(&self, ceilings: BudgetCeilings) -> Result<(), PluginError> {
        self.default.validate("plugin-budgets.default", ceilings)?;
        for (id, values) in &self.plugins {
            values.validate(&format!("plugin-budgets.plugins[\"{id}\"]"), ceilings)?;
        }
        Ok(())
    }
}

/// Resolve the effective budgets for `plugin_id`.
///
/// All four layers are upper bounds; the result is the per-dimension minimum.
/// Manifest values are re-validated defensively (the manifest parser already
/// did so), and both user layers are validated here so a hand-constructed
/// value set cannot bypass the ceiling.
///
/// # Errors
///
/// [`PluginError::Budget`] when any present value is zero, above the ceiling,
/// or when a layer's `warning_ms` is not below its own `wall_ms`.
pub fn resolve_budgets(
    plugin_id: &str,
    ceilings: BudgetCeilings,
    manifest: BudgetValues,
    user_default: BudgetValues,
    user_plugin: BudgetValues,
) -> Result<EffectiveBudgets, PluginError> {
    manifest.validate("limits", ceilings)?;
    user_default.validate("plugin-budgets.default", ceilings)?;
    user_plugin.validate(
        &format!("plugin-budgets.plugins[\"{plugin_id}\"]"),
        ceilings,
    )?;

    let layers = [
        (BudgetSource::Manifest, manifest),
        (BudgetSource::UserDefault, user_default),
        (BudgetSource::UserPlugin, user_plugin),
    ];
    let mut effective = EffectiveBudgets::host_defaults(ceilings);
    for (source, values) in layers {
        if let Some(value) = values.fuel_instructions {
            if value < effective.fuel_instructions {
                effective.fuel_instructions = value;
                effective.sources.fuel_instructions = source;
            }
        }
        if let Some(value) = values.wall_ms {
            if value < effective.wall_ms {
                effective.wall_ms = value;
                effective.sources.wall_ms = source;
            }
        }
        if let Some(value) = values.memory_bytes {
            if value < effective.memory_bytes {
                effective.memory_bytes = value;
                effective.sources.memory_bytes = source;
            }
        }
        if let Some(value) = values.warning_ms {
            if value < effective.warning_ms {
                effective.warning_ms = value;
                effective.sources.warning_ms = source;
            }
        }
    }

    // The advisory threshold must stay below the effective wall bound. This
    // is a tightening derivation, never a suspension rule: it only makes an
    // advisory warning fire earlier when a plugin declared a very tight wall.
    let capped = effective
        .warning_ms
        .min(effective.wall_ms.saturating_sub(1));
    effective.warning_capped_by_wall = capped != effective.warning_ms;
    effective.warning_ms = capped;
    Ok(effective)
}

/// Parse a bounded, data-only user budget file.
///
/// The accepted grammar is:
///
/// ```text
/// return {
///   default = { fuel_instructions = 1000000, wall_ms = 20,
///               memory_bytes = 8388608, warning_ms = 4 },
///   plugins = {
///     ["owner.name"] = { wall_ms = 10 },
///   },
/// }
/// ```
///
/// Tables, keys, and comments (`--`) are supported; everything else — string
/// values, floats, negative numbers, function calls, variables, duplicate
/// keys, unknown keys — fails the whole file closed with a typed
/// [`PluginError::Budget`]. Values are validated against the host ceilings
/// here, so callers receive a usable [`PluginBudgetFile`] or an error.
///
/// # Errors
///
/// [`PluginError::Budget`] with `plugin-budgets.lua` or a precise field path.
pub fn parse_budget_file(bytes: &[u8]) -> Result<PluginBudgetFile, PluginError> {
    if bytes.len() > MAX_BUDGET_FILE_BYTES {
        return Err(PluginError::Budget {
            field: BUDGET_FILE_NAME.to_string(),
            message: format!("file exceeds {MAX_BUDGET_FILE_BYTES} bytes"),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| PluginError::Budget {
        field: BUDGET_FILE_NAME.to_string(),
        message: "file must be valid UTF-8".to_string(),
    })?;
    let lines = text.lines().count();
    if lines > MAX_BUDGET_FILE_LINES {
        return Err(PluginError::Budget {
            field: BUDGET_FILE_NAME.to_string(),
            message: format!("file exceeds {MAX_BUDGET_FILE_LINES} lines"),
        });
    }
    for (index, line) in text.lines().enumerate() {
        if line.len() > MAX_BUDGET_FILE_LINE_BYTES {
            return Err(PluginError::Budget {
                field: BUDGET_FILE_NAME.to_string(),
                message: format!(
                    "line {} exceeds {MAX_BUDGET_FILE_LINE_BYTES} bytes",
                    index + 1
                ),
            });
        }
    }

    let mut parser = BudgetParser {
        bytes: bytes.to_vec(),
        pos: 0,
        line: 1,
    };
    let file = parser.parse_document()?;
    file.validate(BudgetCeilings::HOST)?;
    Ok(file)
}

struct BudgetParser {
    bytes: Vec<u8>,
    pos: usize,
    line: usize,
}

impl BudgetParser {
    fn parse_document(&mut self) -> Result<PluginBudgetFile, PluginError> {
        self.expect_word("return")?;
        self.expect(b'{', "`{` after `return`")?;
        let mut default: Option<BudgetValues> = None;
        let mut plugins: Option<BTreeMap<String, BudgetValues>> = None;
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_bare_key()?;
            self.expect_assign()?;
            match key.as_str() {
                "default" => {
                    if default.is_some() {
                        return Err(self.malformed_at(line, "duplicate key `default`"));
                    }
                    default = Some(self.parse_budget_table("plugin-budgets.default")?);
                }
                "plugins" => {
                    if plugins.is_some() {
                        return Err(self.malformed_at(line, "duplicate key `plugins`"));
                    }
                    plugins = Some(self.parse_plugins()?);
                }
                other => {
                    return Err(
                        self.malformed_at(line, format!("unknown budget file key `{other}`"))
                    );
                }
            }
            self.finish_entry()?;
        }
        self.skip_trivia();
        if self.pos != self.bytes.len() {
            return Err(self.malformed("trailing content after the budget table"));
        }
        Ok(PluginBudgetFile {
            default: default.unwrap_or_default(),
            plugins: plugins.unwrap_or_default(),
        })
    }

    fn parse_plugins(&mut self) -> Result<BTreeMap<String, BudgetValues>, PluginError> {
        self.expect(b'{', "`{` for the `plugins` table")?;
        let mut plugins = BTreeMap::new();
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_plugin_key()?;
            let id = PluginId::new(&key).map_err(|error| PluginError::Budget {
                field: format!("plugin-budgets.plugins[\"{key}\"]"),
                message: error.to_string(),
            })?;
            self.expect_assign()?;
            let values =
                self.parse_budget_table(&format!("plugin-budgets.plugins[\"{}\"]", id.as_str()))?;
            if plugins.insert(id.as_str().to_string(), values).is_some() {
                return Err(self.malformed_at(line, format!("duplicate plugin record `{key}`")));
            }
            if plugins.len() > MAX_BUDGET_PLUGINS {
                return Err(self.malformed_at(
                    line,
                    format!("more than {MAX_BUDGET_PLUGINS} plugin records"),
                ));
            }
            self.finish_entry()?;
        }
        Ok(plugins)
    }

    fn parse_budget_table(&mut self, field_prefix: &str) -> Result<BudgetValues, PluginError> {
        self.expect(b'{', "`{` for a budget table")?;
        let mut values = BudgetValues::default();
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_bare_key()?;
            self.expect_assign()?;
            let value = self.parse_u64(&key)?;
            let slot: &mut Option<u64> = match key.as_str() {
                "fuel_instructions" => &mut values.fuel_instructions,
                "wall_ms" => &mut values.wall_ms,
                "warning_ms" => &mut values.warning_ms,
                "memory_bytes" => {
                    if value > usize::MAX as u64 {
                        return Err(
                            self.malformed_at(line, "memory_bytes is too large for this host")
                        );
                    }
                    if values.memory_bytes.is_some() {
                        return Err(self.malformed_at(line, "duplicate key `memory_bytes`"));
                    }
                    values.memory_bytes = Some(value as usize);
                    self.finish_entry()?;
                    continue;
                }
                other => {
                    return Err(self.malformed_at(line, format!("unknown budget key `{other}`")));
                }
            };
            if slot.is_some() {
                return Err(self.malformed_at(line, format!("duplicate key `{key}`")));
            }
            *slot = Some(value);
            self.finish_entry()?;
        }
        values.validate(field_prefix, BudgetCeilings::HOST)?;
        Ok(values)
    }

    fn finish_entry(&mut self) -> Result<(), PluginError> {
        self.skip_trivia();
        if self.eat(b',') {
            return Ok(());
        }
        if self.peek() == Some(b'}') {
            return Ok(());
        }
        Err(self.malformed("expected `,` or `}`"))
    }

    fn parse_plugin_key(&mut self) -> Result<String, PluginError> {
        self.skip_trivia();
        if self.eat(b'[') {
            let key = self.parse_string()?;
            self.expect(b']', "`]` after a bracketed plugin id")?;
            return Ok(key);
        }
        self.parse_bare_key()
    }

    fn parse_bare_key(&mut self) -> Result<String, PluginError> {
        self.skip_trivia();
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| self.malformed("expected a key"))?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(self.malformed("expected a key"));
        }
        while let Some(byte) = self.peek() {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        String::from_utf8(self.bytes[start..self.pos].to_vec())
            .map_err(|_| self.malformed("key must be UTF-8"))
    }

    fn parse_string(&mut self) -> Result<String, PluginError> {
        self.expect(b'"', "`\"` before a plugin id")?;
        let mut out = String::new();
        loop {
            let byte = self
                .bump()
                .ok_or_else(|| self.malformed("unterminated string"))?;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escaped = self
                        .bump()
                        .ok_or_else(|| self.malformed("unterminated escape"))?;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        other => {
                            return Err(self.malformed(format!("unsupported escape `\\{other}`")));
                        }
                    }
                }
                b'\n' => return Err(self.malformed("string must stay on one line")),
                other if other.is_ascii_control() => {
                    return Err(self.malformed("string must not contain control characters"));
                }
                other => out.push(char::from(other)),
            }
        }
    }

    fn parse_u64(&mut self, key: &str) -> Result<u64, PluginError> {
        self.skip_trivia();
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.malformed(format!("expected a bare non-negative integer for `{key}`")));
        }
        let digits = &self.bytes[start..self.pos];
        if digits.len() > 19 {
            return Err(self.malformed(format!("integer for `{key}` is too large")));
        }
        let text = std::str::from_utf8(digits)
            .map_err(|_| self.malformed(format!("integer for `{key}` is not UTF-8")))?;
        text.parse::<u64>()
            .map_err(|_| self.malformed(format!("integer for `{key}` is too large")))
    }

    fn expect_word(&mut self, word: &str) -> Result<(), PluginError> {
        self.skip_trivia();
        for expected in word.bytes() {
            if self.bump() != Some(expected) {
                return Err(self.malformed(format!("expected `{word}`")));
            }
        }
        if self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(self.malformed(format!("expected `{word}`")));
        }
        Ok(())
    }

    fn expect_assign(&mut self) -> Result<(), PluginError> {
        self.expect(b'=', "`=` after a key")?;
        self.skip_trivia();
        Ok(())
    }

    fn expect(&mut self, byte: u8, what: &str) -> Result<(), PluginError> {
        self.skip_trivia();
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.malformed(format!("expected {what}")))
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.bump();
                }
                Some(b'-') if self.peek_next() == Some(b'-') => {
                    while let Some(byte) = self.bump() {
                        if byte == b'\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        if byte == b'\n' {
            self.line += 1;
        }
        Some(byte)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn malformed(&self, reason: impl Into<String>) -> PluginError {
        self.malformed_at(self.line, reason)
    }

    fn malformed_at(&self, line: usize, reason: impl Into<String>) -> PluginError {
        PluginError::Budget {
            field: BUDGET_FILE_NAME.to_string(),
            message: format!("line {line}: {}", reason.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> BudgetCeilings {
        BudgetCeilings::HOST
    }

    fn values(
        fuel: Option<u64>,
        wall: Option<u64>,
        memory: Option<usize>,
        warning: Option<u64>,
    ) -> BudgetValues {
        BudgetValues {
            fuel_instructions: fuel,
            wall_ms: wall,
            memory_bytes: memory,
            warning_ms: warning,
        }
    }

    #[test]
    fn host_defaults_match_ceilings() {
        let effective = EffectiveBudgets::host_defaults(host());
        assert_eq!(effective.fuel_instructions, HOST_FUEL_INSTRUCTIONS_CEILING);
        assert_eq!(effective.wall_ms, HOST_WALL_MS_CEILING);
        assert_eq!(effective.memory_bytes, HOST_MEMORY_BYTES_CEILING);
        assert_eq!(effective.warning_ms, HOST_WARNING_MS_CEILING);
        assert_eq!(
            effective.sources.fuel_instructions,
            BudgetSource::HostDefault
        );
    }

    #[test]
    fn absent_layers_keep_host_defaults() {
        let effective = resolve_budgets(
            "xuepoo.a",
            host(),
            BudgetValues::default(),
            BudgetValues::default(),
            BudgetValues::default(),
        )
        .expect("empty layers");
        assert_eq!(effective, EffectiveBudgets::host_defaults(host()));
    }

    #[test]
    fn present_values_only_tighten_and_minimum_wins() {
        let manifest = values(Some(5_000_000), Some(40), Some(16 * 1024 * 1024), Some(6));
        let user_default = values(Some(4_000_000), Some(30), None, None);
        let user_plugin = values(None, Some(10), Some(8 * 1024 * 1024), Some(2));
        let effective = resolve_budgets("xuepoo.a", host(), manifest, user_default, user_plugin)
            .expect("tightening resolves");
        assert_eq!(effective.fuel_instructions, 4_000_000);
        assert_eq!(
            effective.sources.fuel_instructions,
            BudgetSource::UserDefault
        );
        assert_eq!(effective.wall_ms, 10);
        assert_eq!(effective.sources.wall_ms, BudgetSource::UserPlugin);
        assert_eq!(effective.memory_bytes, 8 * 1024 * 1024);
        assert_eq!(effective.sources.memory_bytes, BudgetSource::UserPlugin);
        assert_eq!(effective.warning_ms, 2);
        assert_eq!(effective.sources.warning_ms, BudgetSource::UserPlugin);
        assert!(!effective.warning_capped_by_wall);
    }

    #[test]
    fn user_layers_cannot_widen_manifest_or_host() {
        // Above the manifest declaration but inside the host ceiling: inert.
        let manifest = values(Some(1_000_000), Some(10), None, None);
        let user_plugin = values(Some(9_000_000), Some(40), None, None);
        let effective = resolve_budgets(
            "xuepoo.a",
            host(),
            manifest,
            BudgetValues::default(),
            user_plugin,
        )
        .expect("values inside the host ceiling are accepted");
        assert_eq!(effective.fuel_instructions, 1_000_000);
        assert_eq!(effective.wall_ms, 10);
        assert_eq!(effective.sources.fuel_instructions, BudgetSource::Manifest);
        assert_eq!(effective.sources.wall_ms, BudgetSource::Manifest);
    }

    #[test]
    fn zero_and_above_ceiling_fail_closed() {
        for invalid in [
            values(Some(0), None, None, None),
            values(None, Some(0), None, None),
            values(None, None, Some(0), None),
            values(None, None, None, Some(0)),
            values(Some(HOST_FUEL_INSTRUCTIONS_CEILING + 1), None, None, None),
            values(None, Some(HOST_WALL_MS_CEILING + 1), None, None),
            values(None, None, Some(HOST_MEMORY_BYTES_CEILING + 1), None),
            values(None, None, None, Some(HOST_WARNING_MS_CEILING + 1)),
        ] {
            let error = resolve_budgets(
                "xuepoo.a",
                host(),
                invalid,
                BudgetValues::default(),
                BudgetValues::default(),
            )
            .expect_err("invalid budget must fail closed");
            assert!(
                matches!(error, PluginError::Budget { .. }),
                "expected typed budget error, got {error:?}"
            );
        }
    }

    #[test]
    fn warning_must_stay_below_wall_in_the_same_layer() {
        let error = values(None, Some(10), None, Some(10))
            .validate("limits", host())
            .expect_err("warning == wall must fail");
        assert!(error.to_string().contains("limits.warning_ms"), "{error}");
        values(None, Some(10), None, Some(8))
            .validate("limits", host())
            .expect("warning < wall");
    }

    #[test]
    fn warning_is_capped_by_a_tighter_effective_wall() {
        let effective = resolve_budgets(
            "xuepoo.a",
            host(),
            values(None, Some(5), None, None),
            BudgetValues::default(),
            BudgetValues::default(),
        )
        .expect("tight wall");
        assert_eq!(effective.wall_ms, 5);
        assert_eq!(effective.warning_ms, 4);
        assert!(effective.warning_capped_by_wall);
        assert_eq!(effective.sources.warning_ms, BudgetSource::HostDefault);
    }

    #[test]
    fn parses_global_and_per_plugin_tables() {
        let file = parse_budget_file(
            br#"
            -- tightening only
            return {
              default = { fuel_instructions = 4000000, wall_ms = 25 },
              plugins = {
                ["xuepoo.hello"] = { memory_bytes = 8388608, warning_ms = 2 },
              },
            }
            "#,
        )
        .expect("valid budget file");
        assert_eq!(file.default.fuel_instructions, Some(4_000_000));
        assert_eq!(file.default.wall_ms, Some(25));
        assert_eq!(
            file.for_plugin("xuepoo.hello"),
            values(None, None, Some(8 * 1024 * 1024), Some(2))
        );
        assert!(file.for_plugin("xuepoo.other").is_empty());
    }

    #[test]
    fn rejects_wrong_types_negative_and_unknown_keys() {
        for fragment in [
            "fuel_instructions = \"4000\"\n",
            "fuel_instructions = -1\n",
            "fuel_instructions = 4.0\n",
            "fuel_instructions = true\n",
            "fuel_everything = 4\n",
            "fuel_instructions = 4000\nfuel_instructions = 4000\n",
            "fuel_instructions = 40000000\n",
            "wall_ms = 0\n",
            "wall_ms = 10\nwarning_ms = 10\n",
        ] {
            let body = format!("return {{ default = {{ {fragment} }} }}");
            let error = parse_budget_file(body.as_bytes())
                .expect_err(&format!("fragment must fail closed: {fragment:?}"));
            assert!(
                matches!(error, PluginError::Budget { .. }),
                "typed error for {fragment:?}: {error:?}"
            );
        }
    }

    #[test]
    fn rejects_structural_garbage_and_duplicate_plugin_ids() {
        for body in [
            "return { default = { wall_ms = 10 } } trailing",
            "local x = 1\nreturn {}",
            "return { default = { wall_ms = 10 }",
            "return { plugins = { [\"bad id\"] = { wall_ms = 10 } } }",
            "return { plugins = { [\"xuepoo.a\"] = { wall_ms = 10 }, [\"xuepoo.a\"] = { wall_ms = 9 } } }",
        ] {
            let error = parse_budget_file(body.as_bytes())
                .expect_err(&format!("body must fail closed: {body:?}"));
            assert!(matches!(error, PluginError::Budget { .. }), "{error:?}");
        }
    }

    #[test]
    fn accepts_empty_file_bounds() {
        for body in ["return {}", "return { default = {}, plugins = {} }"] {
            let file = parse_budget_file(body.as_bytes()).expect("empty file is valid");
            assert!(file.default.is_empty());
            assert!(file.plugins.is_empty());
        }
    }

    #[test]
    fn oversize_and_non_utf8_fail_closed() {
        let oversize = vec![b' '; MAX_BUDGET_FILE_BYTES + 1];
        assert!(parse_budget_file(&oversize).is_err());
        assert!(parse_budget_file(&[0xff, 0xfe]).is_err());
        let many_lines = "return {\n".to_string() + &"--\n".repeat(MAX_BUDGET_FILE_LINES) + "}\n";
        assert!(parse_budget_file(many_lines.as_bytes()).is_err());
    }
}
