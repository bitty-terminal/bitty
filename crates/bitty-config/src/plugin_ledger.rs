//! Data-only Lua ledger for the machine-managed plugin state (CTX-1487,
//! issue #2576).
//!
//! This module owns the **bounded parser** for
//! `$XDG_CONFIG_HOME/bitty/bitty-plugins.lua` (fallback
//! `~/.config/bitty/bitty-plugins.lua`). The file replaces the former TOML
//! ledger with the same records:
//!
//! ```lua
//! return {
//!     state_version = 1,
//!     plugins = {
//!         ["owner.name"] = {
//!             source = "bundled",
//!             manifest_hash = "<64 lowercase hex>",
//!             enabled = true,
//!             granted = { "terminal.semantic-read" },
//!             denied = {},
//!         },
//!     },
//! }
//! ```
//!
//! The ledger is **data, never code**. This parser does not embed a Lua VM:
//! it accepts exactly one `return` statement whose value is a table literal
//! with the ledger shape above. There are no function calls, operators,
//! variables, concatenation, indexing, loops, or long strings. Comments are
//! `--` to end of line. Any other token fails the whole file closed.
//!
//! # Bounds (fail closed before any value is trusted)
//!
//! - File bytes `<= MAX_LEDGER_FILE_BYTES` (64 KiB).
//! - Lines `<= MAX_LEDGER_FILE_LINES` (4096), each
//!   `<= MAX_LEDGER_LINE_BYTES` (1024).
//! - Plugins `<= MAX_LEDGER_PLUGINS` (256).
//! - Capabilities per set `<= MAX_LEDGER_CAPABILITIES` (256).
//! - Keys `<= MAX_LEDGER_KEY_BYTES` (256); strings
//!   `<= MAX_LEDGER_STRING_BYTES` (512).
//! - UTF-8 only. NUL and raw control characters are rejected in strings; the
//!   `\n`, `\r`, and `\t` escapes decode to their control characters, which
//!   the caller's identifier/hash grammars then reject, so no such value can
//!   reach ledger state. No duplicate keys or array items, and no
//!   `state_version` other than [`LEDGER_STATE_VERSION`].
//!
//! # Responsibility split
//!
//! The parser returns raw strings. Identifier grammars, the closed source
//! set, capability validation, and manifest-hash binding belong to the
//! caller (`bitty-terminal`'s plugin state layer), which applies them before
//! any record can affect authority. Nothing parsed here grants anything by
//! itself.
//!
//! Pure `std`, no I/O, no workspace-crate dependencies; headlessly testable
//! on Linux CI and the `windows-latest` job.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Canonical file name of the machine-managed Lua ledger.
pub const LEDGER_FILE_NAME: &str = "bitty-plugins.lua";

/// Former TOML ledger file name, read only for one-time migration.
pub const LEGACY_LEDGER_FILE_NAME: &str = "bitty-plugins.toml";

/// Ledger schema version this parser accepts.
pub const LEDGER_STATE_VERSION: u32 = 1;

/// Maximum ledger file size in bytes (fail-closed).
pub const MAX_LEDGER_FILE_BYTES: usize = 64 * 1024;

/// Maximum ledger lines (fail-closed).
pub const MAX_LEDGER_FILE_LINES: usize = 4096;

/// Maximum bytes per ledger line (fail-closed).
pub const MAX_LEDGER_LINE_BYTES: usize = 1024;

/// Maximum plugin records (fail-closed).
pub const MAX_LEDGER_PLUGINS: usize = 256;

/// Maximum capabilities per granted/denied set (fail-closed).
pub const MAX_LEDGER_CAPABILITIES: usize = 256;

/// Maximum bytes of one ledger key (fail-closed).
pub const MAX_LEDGER_KEY_BYTES: usize = 256;

/// Maximum bytes of one ledger string value (fail-closed).
pub const MAX_LEDGER_STRING_BYTES: usize = 512;

/// One plugin record as written in the Lua ledger (untyped strings).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LedgerPlugin {
    /// Provenance label (validated by the caller; `bundled` today).
    pub source: String,
    /// Hex-encoded manifest hash the record is bound to.
    pub manifest_hash: String,
    /// Recorded desired enabled state.
    pub enabled: bool,
    /// Granted capability identifiers.
    pub granted: Vec<String>,
    /// Explicitly revoked (denied) capability identifiers.
    pub denied: Vec<String>,
}

/// Parsed Lua ledger (bounded, unvalidated identifiers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLedger {
    /// Ledger schema version (always [`LEDGER_STATE_VERSION`]).
    pub state_version: u32,
    /// Records keyed by plugin id, sorted deterministically.
    pub plugins: BTreeMap<String, LedgerPlugin>,
}

impl Default for PluginLedger {
    fn default() -> Self {
        Self {
            state_version: LEDGER_STATE_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

/// Fail-closed ledger parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// File exceeds [`MAX_LEDGER_FILE_BYTES`].
    Oversize {
        /// Actual byte length.
        actual: usize,
    },
    /// File exceeds [`MAX_LEDGER_FILE_LINES`].
    TooManyLines {
        /// Actual line count (at least the ceiling + 1).
        actual: usize,
    },
    /// One line exceeds [`MAX_LEDGER_LINE_BYTES`].
    LineTooLong {
        /// 1-based line number.
        line: usize,
    },
    /// File is not valid UTF-8 (no lossy reads, fail closed).
    InvalidUtf8,
    /// Structural garbage, an unsupported token, a duplicate key/item, an
    /// unknown field, an over-limit collection, or an unsupported version.
    Malformed {
        /// 1-based line number (0 when the problem is not line-attributable).
        line: usize,
        /// Bounded reason (never echoes file bytes beyond a key name).
        reason: String,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { actual } => write!(
                f,
                "plugin ledger exceeds {MAX_LEDGER_FILE_BYTES} bytes ({actual} bytes)"
            ),
            Self::TooManyLines { actual } => write!(
                f,
                "plugin ledger exceeds {MAX_LEDGER_FILE_LINES} lines ({actual} lines)"
            ),
            Self::LineTooLong { line } => write!(
                f,
                "plugin ledger line {line} exceeds {MAX_LEDGER_LINE_BYTES} bytes"
            ),
            Self::InvalidUtf8 => write!(f, "plugin ledger must be valid UTF-8"),
            Self::Malformed { line: 0, reason } => {
                write!(f, "plugin ledger malformed: {reason}")
            }
            Self::Malformed { line, reason } => {
                write!(f, "plugin ledger malformed at line {line}: {reason}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// Parse ledger bytes into [`PluginLedger`].
///
/// Pure and headless: the caller supplies the already-read bytes (no I/O, no
/// environment). Every problem is a whole-file [`LedgerError`]; a ledger is
/// either fully valid or not trusted at all.
///
/// # Errors
///
/// Returns [`LedgerError`] for over-limit files or lines, non-UTF-8 input,
/// syntax outside the data-only subset, duplicate keys/items, unknown
/// fields, or an unsupported `state_version`.
pub fn parse_ledger(bytes: &[u8]) -> Result<PluginLedger, LedgerError> {
    if bytes.len() > MAX_LEDGER_FILE_BYTES {
        return Err(LedgerError::Oversize {
            actual: bytes.len(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| LedgerError::InvalidUtf8)?;
    let mut line_count = 0usize;
    for line in text.split('\n') {
        line_count += 1;
        if line_count > MAX_LEDGER_FILE_LINES {
            return Err(LedgerError::TooManyLines { actual: line_count });
        }
        if line.len() > MAX_LEDGER_LINE_BYTES {
            return Err(LedgerError::LineTooLong { line: line_count });
        }
    }
    let mut parser = Parser {
        bytes: bytes.to_vec(),
        pos: 0,
        line: 1,
    };
    parser.parse_document()
}

// ---------------------------------------------------------------------------
// Bounded data-only parser
// ---------------------------------------------------------------------------

struct Parser {
    bytes: Vec<u8>,
    pos: usize,
    line: usize,
}

impl Parser {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        if byte == b'\n' {
            self.line += 1;
        }
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn malformed(&self, reason: impl Into<String>) -> LedgerError {
        LedgerError::Malformed {
            line: self.line,
            reason: reason.into(),
        }
    }

    fn malformed_at(line: usize, reason: impl Into<String>) -> LedgerError {
        LedgerError::Malformed {
            line,
            reason: reason.into(),
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

    fn expect(&mut self, byte: u8, what: &str) -> Result<(), LedgerError> {
        self.skip_trivia();
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.malformed(format!("expected {what}")))
        }
    }

    fn expect_word(&mut self, word: &str) -> Result<(), LedgerError> {
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

    fn expect_assign(&mut self) -> Result<(), LedgerError> {
        self.expect(b'=', "`=` after a key")?;
        self.skip_trivia();
        Ok(())
    }

    fn parse_document(&mut self) -> Result<PluginLedger, LedgerError> {
        self.expect_word("return")?;
        self.expect(b'{', "`{` after `return`")?;
        let mut state_version: Option<u32> = None;
        let mut plugins: Option<BTreeMap<String, LedgerPlugin>> = None;
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_key()?;
            self.expect_assign()?;
            match key.as_str() {
                "state_version" => {
                    if state_version.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `state_version`"));
                    }
                    let value = self.parse_integer()?;
                    if value != i64::from(LEDGER_STATE_VERSION) {
                        return Err(Self::malformed_at(
                            line,
                            format!(
                                "unsupported state_version `{value}` (want {LEDGER_STATE_VERSION})"
                            ),
                        ));
                    }
                    state_version = Some(LEDGER_STATE_VERSION);
                }
                "plugins" => {
                    if plugins.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `plugins`"));
                    }
                    plugins = Some(self.parse_plugins()?);
                }
                other => {
                    return Err(Self::malformed_at(
                        line,
                        format!("unknown ledger key `{other}`"),
                    ));
                }
            }
            self.skip_trivia();
            if self.eat(b',') {
                continue;
            }
            if self.peek() == Some(b'}') {
                continue;
            }
            return Err(self.malformed("expected `,` or `}`"));
        }
        self.skip_trivia();
        if self.pos != self.bytes.len() {
            return Err(self.malformed("trailing content after the ledger table"));
        }
        if state_version.is_none() {
            return Err(Self::malformed_at(1, "missing `state_version`"));
        }
        let plugins = plugins.ok_or_else(|| Self::malformed_at(1, "missing `plugins` table"))?;
        Ok(PluginLedger {
            state_version: LEDGER_STATE_VERSION,
            plugins,
        })
    }

    fn parse_plugins(&mut self) -> Result<BTreeMap<String, LedgerPlugin>, LedgerError> {
        self.expect(b'{', "`{` for the plugins table")?;
        let mut plugins = BTreeMap::new();
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_key()?;
            self.expect_assign()?;
            let record = self.parse_record()?;
            if plugins.insert(key.clone(), record).is_some() {
                return Err(Self::malformed_at(
                    line,
                    format!("duplicate plugin record `{key}`"),
                ));
            }
            if plugins.len() > MAX_LEDGER_PLUGINS {
                return Err(Self::malformed_at(
                    line,
                    format!("more than {MAX_LEDGER_PLUGINS} plugin records"),
                ));
            }
            self.skip_trivia();
            if self.eat(b',') {
                continue;
            }
            if self.peek() == Some(b'}') {
                continue;
            }
            return Err(self.malformed("expected `,` or `}`"));
        }
        Ok(plugins)
    }

    fn parse_record(&mut self) -> Result<LedgerPlugin, LedgerError> {
        self.expect(b'{', "`{` for a plugin record")?;
        let mut source: Option<String> = None;
        let mut manifest_hash: Option<String> = None;
        let mut enabled: Option<bool> = None;
        let mut granted: Option<Vec<String>> = None;
        let mut denied: Option<Vec<String>> = None;
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            let key = self.parse_key()?;
            self.expect_assign()?;
            match key.as_str() {
                "source" => {
                    if source.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `source`"));
                    }
                    source = Some(self.parse_string()?);
                }
                "manifest_hash" => {
                    if manifest_hash.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `manifest_hash`"));
                    }
                    manifest_hash = Some(self.parse_string()?);
                }
                "enabled" => {
                    if enabled.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `enabled`"));
                    }
                    enabled = Some(self.parse_bool()?);
                }
                "granted" => {
                    if granted.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `granted`"));
                    }
                    granted = Some(self.parse_string_array()?);
                }
                "denied" => {
                    if denied.is_some() {
                        return Err(Self::malformed_at(line, "duplicate key `denied`"));
                    }
                    denied = Some(self.parse_string_array()?);
                }
                other => {
                    return Err(Self::malformed_at(
                        line,
                        format!("unknown plugin record key `{other}`"),
                    ));
                }
            }
            self.skip_trivia();
            if self.eat(b',') {
                continue;
            }
            if self.peek() == Some(b'}') {
                continue;
            }
            return Err(self.malformed("expected `,` or `}`"));
        }
        let source =
            source.ok_or_else(|| Self::malformed_at(self.line, "record is missing `source`"))?;
        let manifest_hash = manifest_hash
            .ok_or_else(|| Self::malformed_at(self.line, "record is missing `manifest_hash`"))?;
        let enabled =
            enabled.ok_or_else(|| Self::malformed_at(self.line, "record is missing `enabled`"))?;
        let granted =
            granted.ok_or_else(|| Self::malformed_at(self.line, "record is missing `granted`"))?;
        Ok(LedgerPlugin {
            source,
            manifest_hash,
            enabled,
            granted,
            denied: denied.unwrap_or_default(),
        })
    }

    fn parse_key(&mut self) -> Result<String, LedgerError> {
        self.skip_trivia();
        // Lua table-constructor keys are bare (`state_version`) or bracketed
        // string literals (`["owner.name"]`). Nothing else is a key: a
        // variable, expression, or bare dotted name is outside the subset.
        if self.eat(b'[') {
            let key = self.parse_string()?;
            self.expect(b']', "`]` after a bracketed key")?;
            if key.len() > MAX_LEDGER_KEY_BYTES {
                return Err(self.malformed(format!("key exceeds {MAX_LEDGER_KEY_BYTES} bytes")));
            }
            return Ok(key);
        }
        if self.peek() == Some(b'"') {
            let key = self.parse_string()?;
            if key.len() > MAX_LEDGER_KEY_BYTES {
                return Err(self.malformed(format!("key exceeds {MAX_LEDGER_KEY_BYTES} bytes")));
            }
            return Ok(key);
        }
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| self.malformed("expected a key"))?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(self.malformed("expected a key"));
        }
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            self.bump();
        }
        let slice = &self.bytes[start..self.pos];
        if slice.len() > MAX_LEDGER_KEY_BYTES {
            return Err(self.malformed(format!("key exceeds {MAX_LEDGER_KEY_BYTES} bytes")));
        }
        let key = std::str::from_utf8(slice)
            .map_err(|_| self.malformed("key is not valid UTF-8"))?
            .to_string();
        Ok(key)
    }

    fn parse_integer(&mut self) -> Result<i64, LedgerError> {
        self.skip_trivia();
        let start = self.pos;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.bump();
        }
        if start == self.pos {
            return Err(self.malformed("expected an integer"));
        }
        let digits = &self.bytes[start..self.pos];
        if digits.len() > 19 {
            return Err(self.malformed("integer is too large"));
        }
        let text = std::str::from_utf8(digits)
            .map_err(|_| self.malformed("integer is not valid UTF-8"))?;
        text.parse::<i64>()
            .map_err(|_| self.malformed("integer is too large"))
    }

    fn parse_bool(&mut self) -> Result<bool, LedgerError> {
        self.skip_trivia();
        if self.starts_with_word("true") {
            self.expect_word("true")?;
            return Ok(true);
        }
        if self.starts_with_word("false") {
            self.expect_word("false")?;
            return Ok(false);
        }
        Err(self.malformed("expected `true` or `false`"))
    }

    fn starts_with_word(&self, word: &str) -> bool {
        let end = self.pos + word.len();
        if end > self.bytes.len() || &self.bytes[self.pos..end] != word.as_bytes() {
            return false;
        }
        !self
            .bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    }

    fn parse_string_array(&mut self) -> Result<Vec<String>, LedgerError> {
        self.expect(b'{', "`{` for a capability set")?;
        let mut items = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        loop {
            self.skip_trivia();
            if self.eat(b'}') {
                break;
            }
            let line = self.line;
            if self.peek() != Some(b'"') {
                return Err(Self::malformed_at(
                    line,
                    "capability items must be double-quoted strings",
                ));
            }
            let item = self.parse_string()?;
            if !seen.insert(item.clone()) {
                return Err(Self::malformed_at(
                    line,
                    format!("duplicate capability `{item}`"),
                ));
            }
            items.push(item);
            if items.len() > MAX_LEDGER_CAPABILITIES {
                return Err(Self::malformed_at(
                    line,
                    format!("more than {MAX_LEDGER_CAPABILITIES} capabilities"),
                ));
            }
            self.skip_trivia();
            if self.eat(b',') {
                continue;
            }
            if self.peek() == Some(b'}') {
                continue;
            }
            return Err(self.malformed("expected `,` or `}`"));
        }
        Ok(items)
    }

    fn parse_string(&mut self) -> Result<String, LedgerError> {
        self.skip_trivia();
        let start_line = self.line;
        if !self.eat(b'"') {
            return Err(self.malformed("expected a double-quoted string"));
        }
        let mut out = Vec::new();
        loop {
            let Some(byte) = self.bump() else {
                return Err(Self::malformed_at(start_line, "unterminated string"));
            };
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.bump() else {
                        return Err(Self::malformed_at(start_line, "truncated escape"));
                    };
                    match escape {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        _ => {
                            return Err(Self::malformed_at(self.line, "unsupported string escape"));
                        }
                    }
                }
                b'\n' | b'\r' => {
                    return Err(Self::malformed_at(start_line, "newline inside a string"));
                }
                byte if byte < b' ' || byte == 0x7f => {
                    return Err(Self::malformed_at(
                        self.line,
                        "control character inside a string",
                    ));
                }
                byte => out.push(byte),
            }
            if out.len() > MAX_LEDGER_STRING_BYTES {
                return Err(Self::malformed_at(
                    self.line,
                    format!("string exceeds {MAX_LEDGER_STRING_BYTES} bytes"),
                ));
            }
        }
        let text = String::from_utf8(out)
            .map_err(|_| Self::malformed_at(start_line, "string is not valid UTF-8"))?;
        if text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
        {
            return Err(Self::malformed_at(
                start_line,
                "control character inside a string",
            ));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        "return {\n\
             \x20   state_version = 1,\n\
             \x20   plugins = {\n\
             \x20       [\"bitty-terminal.workspace\"] = {\n\
             \x20           source = \"bundled\",\n\
             \x20           manifest_hash = \"ab\",\n\
             \x20           enabled = true,\n\
             \x20           granted = { \"terminal.semantic-read\" },\n\
             \x20           denied = {},\n\
             \x20       },\n\
             \x20   },\n\
             }\n"
        .to_string()
    }

    #[test]
    fn parses_the_rendered_shape() {
        let ledger = parse_ledger(sample().as_bytes()).expect("sample parses");
        assert_eq!(ledger.state_version, LEDGER_STATE_VERSION);
        assert_eq!(ledger.plugins.len(), 1);
        let record = ledger
            .plugins
            .get("bitty-terminal.workspace")
            .expect("record");
        assert_eq!(record.source, "bundled");
        assert_eq!(record.manifest_hash, "ab");
        assert!(record.enabled);
        assert_eq!(record.granted, vec!["terminal.semantic-read".to_string()]);
        assert!(record.denied.is_empty());
    }

    #[test]
    fn disabled_record_with_denial_parses() {
        let body = "-- header\n\
                    return {\n\
                    \x20   state_version = 1,\n\
                    \x20   plugins = {\n\
                    \x20       [\"bitty-terminal.shell-integration\"] = {\n\
                    \x20           source = \"bundled\",\n\
                    \x20           manifest_hash = \"108fe168598d60217e25d55062db6d0370af3cdd3a35683351c896145efd85b2\",\n\
                    \x20           enabled = false,\n\
                    \x20           granted = {},\n\
                    \x20           denied = { \"terminal.semantic-read\" },\n\
                    \x20       },\n\
                    \x20   },\n\
                    }\n";
        let ledger = parse_ledger(body.as_bytes()).expect("disabled record parses");
        let record = ledger
            .plugins
            .get("bitty-terminal.shell-integration")
            .expect("record");
        assert!(!record.enabled);
        assert_eq!(record.denied, vec!["terminal.semantic-read".to_string()]);
    }

    #[test]
    fn comments_blank_lines_and_trailing_commas_parse() {
        let body = "-- leading comment\n\nreturn { -- inline\n  state_version = 1, -- version\n\
                    plugins = { }, -- empty\n}\n-- trailing comment\n";
        let ledger = parse_ledger(body.as_bytes()).expect("comments parse");
        assert!(ledger.plugins.is_empty());
    }

    #[test]
    fn empty_and_quoted_keys_parse() {
        let body = "return { [\"state_version\"] = 1, [\"plugins\"] = {} }\n";
        let ledger = parse_ledger(body.as_bytes()).expect("quoted keys parse");
        assert!(ledger.plugins.is_empty());

        let body = "return { state_version = 1, plugins = { [\"owner.name\"] = { \
                    source = \"bundled\", manifest_hash = \"00\", enabled = false, \
                    granted = {} } } }\n";
        let ledger = parse_ledger(body.as_bytes()).expect("record without denied parses");
        let record = ledger.plugins.get("owner.name").expect("record");
        assert!(!record.enabled);
        assert!(record.denied.is_empty());
    }

    #[test]
    fn duplicates_fail_closed() {
        for body in [
            "return { state_version = 1, state_version = 1, plugins = {} }\n",
            "return { state_version = 1, plugins = {}, plugins = {} }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\", source = \"bundled\", manifest_hash = \"00\", \
              enabled = true, granted = {} } } }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\", manifest_hash = \"00\", enabled = true, \
              granted = { \"x\", \"x\" } } } }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\", manifest_hash = \"00\", enabled = true, \
              granted = {} }, [\"a.b\"] = { source = \"bundled\", \
              manifest_hash = \"00\", enabled = true, granted = {} } } }\n",
        ] {
            assert!(parse_ledger(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn code_and_unknown_shapes_fail_closed() {
        for body in [
            "return { state_version = 1, plugins = {} } os.execute('id')\n",
            "return { state_version = 1, plugins = {} }; os.execute('id')\n",
            "return { state_version = 1, plugins = {} .. function() end }\n",
            "return { state_version = 1, plugins = 1 + 1 }\n",
            "return { state_version = 1, plugins = load('x') }\n",
            "return { state_version = 2, plugins = {} }\n",
            "return { plugins = {} }\n",
            "return { state_version = 1 }\n",
            "return { state_version = 1, plugins = {}, extra = true }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\", manifest_hash = \"00\", enabled = true, \
              granted = {}, capabilities = true } } }\n",
            "state_version = 1\n",
            "",
        ] {
            assert!(parse_ledger(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn non_data_string_forms_fail_closed() {
        for body in [
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = [[bundled]], manifest_hash = \"00\", enabled = true, \
              granted = {} } } }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\\x00\", manifest_hash = \"00\", enabled = true, \
              granted = {} } } }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bundled\\q\", manifest_hash = \"00\", enabled = true, \
              granted = {} } } }\n",
            "return { state_version = 1, plugins = { [\"a.b\"] = { \
              source = \"bun\ndled\", manifest_hash = \"00\", enabled = true, \
              granted = {} } } }\n",
        ] {
            assert!(parse_ledger(body.as_bytes()).is_err(), "{body:?}");
        }
    }

    #[test]
    fn bounds_fail_closed() {
        let big = vec![b'x'; MAX_LEDGER_FILE_BYTES + 1];
        assert!(matches!(
            parse_ledger(&big),
            Err(LedgerError::Oversize { .. })
        ));

        let mut lines = String::from("return {\n");
        for _ in 0..MAX_LEDGER_FILE_LINES {
            lines.push_str("-- pad\n");
        }
        assert!(matches!(
            parse_ledger(lines.as_bytes()),
            Err(LedgerError::TooManyLines { .. })
        ));

        let long = format!(
            "return {{ state_version = 1, plugins = {{}} }} -- {}\n",
            "x".repeat(MAX_LEDGER_LINE_BYTES)
        );
        assert!(matches!(
            parse_ledger(long.as_bytes()),
            Err(LedgerError::LineTooLong { .. })
        ));

        assert_eq!(
            parse_ledger(&[0x72, 0x65, 0xff, 0x0a]),
            Err(LedgerError::InvalidUtf8)
        );

        let long_string = format!(
            "return {{ state_version = 1, plugins = {{ [\"a.b\"] = {{ \
             source = \"{}\", manifest_hash = \"00\", enabled = true, granted = {{}} }} }} }}\n",
            "s".repeat(MAX_LEDGER_STRING_BYTES + 1)
        );
        assert!(parse_ledger(long_string.as_bytes()).is_err());

        let mut crowded = String::from("return { state_version = 1, plugins = {");
        for index in 0..=MAX_LEDGER_CAPABILITIES {
            if index > 0 {
                crowded.push(',');
            }
            crowded.push_str(&format!("\"c{index}\""));
        }
        crowded.push_str("} }\n");
        assert!(parse_ledger(crowded.as_bytes()).is_err());
    }

    #[test]
    fn display_messages_are_bounded() {
        let error = parse_ledger(b"return {").expect_err("incomplete");
        let text = error.to_string();
        assert!(text.contains("plugin ledger malformed"), "{text}");
    }
}
