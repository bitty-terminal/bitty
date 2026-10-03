//! Bounded, atomic, quota-enforced plugin store (`bitty.store.*`, RFC Gap C).
//!
//! The store is plugin-scoped and survives suspension, reload, and generation
//! disposal. Entries persist as JSON under the platform data directory
//! (`$XDG_DATA_HOME/bitty/plugins-state/<plugin-id>/store.json`); the
//! durable commit behind [`PluginStore::set`] is owned by the injected
//! [`KvCommitBackend`] (W-146 seam). The quota and value bounds are enforced
//! before any mutation; overflow fails closed with `E_STORE_QUOTA` and there
//! is no eviction and no partial write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue};

/// Content-free durable-commit failure: kinds only, safe for logs.
///
/// Validation denials keep their typed `E_STORE_*` codes at the call site;
/// every backend failure surfaces here and maps to `E_STORE_IO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvCommitError {
    message: String,
}

impl KvCommitError {
    /// Builds an I/O denial with a bounded, content-free message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for KvCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store commit: {}", self.message)
    }
}

impl std::error::Error for KvCommitError {}

/// Core-owned KV durable-commit backend (W-146 integration seam).
///
/// Quota and key/value validation run in Core before any backend call, and
/// the candidate image is fully encoded before the commit, so a denied or
/// failed commit leaves both the in-memory state and the previously
/// committed file untouched (no eviction, no partial write).
///
/// The dependency is one-way: Core defines this trait and never imports the
/// extension crate. The application wiring crate implements it with the
/// extracted storage mechanics and injects it via
/// [`PluginStore::with_backend`] / [`PluginStore::load_with_backend`].
/// With no backend injected, path-backed writes fail closed with
/// `E_STORE_IO` and loads start clean and empty.
pub trait KvCommitBackend: Send + Sync + std::fmt::Debug {
    /// Atomically commits the whole store image; failure leaves the
    /// previous destination untouched.
    ///
    /// # Errors
    ///
    /// Returns [`KvCommitError`] (content-free) on over-ceiling payloads or
    /// filesystem failures.
    fn commit_store_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), KvCommitError>;

    /// Reads the whole store image with a hard size cap; `None` is a
    /// missing file (quiet clean start, never an error).
    ///
    /// # Errors
    ///
    /// Returns [`KvCommitError`] (content-free) on over-ceiling or
    /// unreadable files.
    fn load_store_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, KvCommitError>;
}

/// Maximum stored value bytes per entry.
pub const STORE_MAX_VALUE_BYTES: usize = 8 * 1024;
/// Maximum entries per plugin store.
pub const STORE_MAX_ENTRIES: usize = 256;
/// Aggregate store byte budget per plugin (RC-11 candidate).
pub const STORE_MAX_TOTAL_BYTES: usize = 64 * 1024;
/// Maximum store key bytes.
pub const STORE_MAX_KEY_BYTES: usize = 128;
/// Maximum store file bytes accepted on load.
pub const STORE_FILE_MAX_BYTES: usize =
    STORE_MAX_TOTAL_BYTES + STORE_MAX_ENTRIES * STORE_MAX_KEY_BYTES + 4096;
/// Maximum JSON recursion depth accepted by parser and value validation.
pub const JSON_MAX_DEPTH: usize = 16;

/// One plugin's bounded key/value store.
#[derive(Debug)]
pub struct PluginStore {
    path: Option<PathBuf>,
    entries: BTreeMap<String, LuaValue>,
    backend: Option<Arc<dyn KvCommitBackend>>,
}

impl PluginStore {
    /// Create an in-memory store with no persistence path.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: BTreeMap::new(),
            backend: None,
        }
    }

    /// Create an empty store that commits to `path` through `backend`.
    ///
    /// `path: None` disables persistence (in-memory behavior); a store with
    /// a path but no backend (`backend: None`) fails every write closed
    /// with `E_STORE_IO` and loads clean and empty.
    #[must_use]
    pub fn with_backend(path: Option<PathBuf>, backend: Option<Arc<dyn KvCommitBackend>>) -> Self {
        Self {
            path,
            entries: BTreeMap::new(),
            backend,
        }
    }

    /// Load a store from `path` through `backend`, or start empty when the
    /// file is absent.
    ///
    /// # Errors
    ///
    /// Returns a bounded message when the backend is missing, the file
    /// exists but is unreadable, over the file ceiling, or not the JSON
    /// subset this module writes.
    pub fn load_with_backend(
        path: PathBuf,
        backend: Option<Arc<dyn KvCommitBackend>>,
    ) -> Result<Self, String> {
        let bytes = match &backend {
            None => None,
            Some(commits) => commits
                .load_store_bytes(&path)
                .map_err(|e| format!("store read: {e}"))?,
        };
        let Some(bytes) = bytes else {
            return Ok(Self {
                path: Some(path),
                entries: BTreeMap::new(),
                backend,
            });
        };
        if bytes.len() > STORE_FILE_MAX_BYTES {
            return Err("plugin store exceeds the file ceiling".to_string());
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| "store image is not UTF-8".to_string())?;
        let value = parse_json(text).map_err(|e| format!("store parse: {e}"))?;
        let mut entries = BTreeMap::new();
        if let LuaValue::Table(pairs) = value {
            for (key, entry) in pairs {
                let LuaValue::String(key) = key else {
                    return Err("store keys must be strings".to_string());
                };
                validate_entry(&key, &entry).map_err(|err| {
                    format!("store entry '{key}' violates invariants: {}", err.message)
                })?;
                entries.insert(key, entry);
            }
        } else {
            return Err("store root must be an object".to_string());
        }
        validate_store_quota(&entries)
            .map_err(|err| format!("store quota violated: {}", err.message))?;
        Ok(Self {
            path: Some(path),
            entries,
            backend,
        })
    }

    /// Read a value; `None` when absent.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<LuaValue> {
        self.entries.get(key).cloned()
    }

    /// Atomically set (`Some`) or delete (`None`/`Nil`) one entry.
    ///
    /// # Errors
    ///
    /// Fails closed with a typed `E_STORE_*`/`E_TIMEOUT` style error before
    /// any mutation when the key, value, or quota is invalid.
    ///
    /// Candidate entries are persisted before publishing to in-memory state;
    /// on persistence failure, in-memory state and the previous committed
    /// file remain untouched (TERM-RUN-003).
    pub fn set(&mut self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        validate_key(key)?;
        if matches!(value, LuaValue::Nil) {
            let mut candidate = self.entries.clone();
            candidate.remove(key);
            self.persist_entries(&candidate)?;
            self.entries = candidate;
            return Ok(());
        }
        validate_entry(key, &value)?;

        let mut candidate = self.entries.clone();
        candidate.insert(key.to_string(), value);
        validate_store_quota(&candidate)?;
        self.persist_entries(&candidate)?;
        self.entries = candidate;
        Ok(())
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Persistence path, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn persist_entries(&self, candidate: &BTreeMap<String, LuaValue>) -> Result<(), BridgeError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let Some(backend) = &self.backend else {
            return Err(BridgeError::new(
                "runtime",
                "E_STORE_IO",
                "no commit backend for the plugin state file",
            ));
        };
        let mut buffer = String::from("{");
        for (index, (key, value)) in candidate.iter().enumerate() {
            if index > 0 {
                buffer.push(',');
            }
            buffer.push_str(&encode_json(&LuaValue::String(key.clone())));
            buffer.push(':');
            buffer.push_str(&encode_json(value));
        }
        buffer.push('}');
        // Only the durable commit I/O is reported to the RC-1 budget clock;
        // the validation, clone, quota check, and JSON encoding above stay
        // charged to the callback. The bridge credits the report only when
        // this `store.set` succeeds (bitty #1518).
        let commit_started = Instant::now();
        backend
            .commit_store_bytes(path, buffer.as_bytes())
            .map_err(|_| {
                BridgeError::new(
                    "runtime",
                    "E_STORE_IO",
                    "could not commit the plugin state file",
                )
            })?;
        bitty_lua::record_store_commit_io(commit_started.elapsed());
        Ok(())
    }
}

/// Validate an individual key and entry value against store invariants.
///
/// Ensures valid key format, value byte size within [`STORE_MAX_VALUE_BYTES`],
/// and semantic value structure (finite numbers, valid UTF-8 strings, valid table
/// keys, and nesting depth within [`JSON_MAX_DEPTH`]).
pub fn validate_entry(key: &str, value: &LuaValue) -> Result<(), BridgeError> {
    validate_key(key)?;
    let encoded = encode_json(value);
    if encoded.len() > STORE_MAX_VALUE_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_STORE_VALUE_INVALID",
            "store value exceeds the 8 KiB ceiling",
        ));
    }
    validate_json_value(value, 2)?;
    Ok(())
}

/// Validate the aggregate quota (entry count and total encoded bytes) of the store.
pub fn validate_store_quota(entries: &BTreeMap<String, LuaValue>) -> Result<(), BridgeError> {
    if entries.len() > STORE_MAX_ENTRIES {
        return Err(BridgeError::new(
            "budget",
            "E_STORE_QUOTA",
            "plugin store entry count exceeds limit",
        ));
    }
    let total: usize = entries
        .iter()
        .map(|(k, v)| k.len() + encode_json(v).len())
        .sum();
    if total > STORE_MAX_TOTAL_BYTES {
        return Err(BridgeError::new(
            "budget",
            "E_STORE_QUOTA",
            "plugin store quota exceeded",
        ));
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), BridgeError> {
    if key.is_empty() || key.len() > STORE_MAX_KEY_BYTES {
        return Err(BridgeError::new(
            "validation",
            "E_STORE_KEY_INVALID",
            "store key must be 1..128 bytes",
        ));
    }
    let bytes = key.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(BridgeError::new(
            "validation",
            "E_STORE_KEY_INVALID",
            "store key must start with [a-z0-9]",
        ));
    }
    for byte in bytes {
        let allowed = byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'.' | b'_');
        if !allowed {
            return Err(BridgeError::new(
                "validation",
                "E_STORE_KEY_INVALID",
                "store key contains an invalid character",
            ));
        }
    }
    if key.contains("..") {
        return Err(BridgeError::new(
            "validation",
            "E_STORE_KEY_INVALID",
            "store key must not contain empty dot segments",
        ));
    }
    Ok(())
}

fn validate_json_value(value: &LuaValue, depth: usize) -> Result<(), BridgeError> {
    if depth > JSON_MAX_DEPTH {
        return Err(BridgeError::new(
            "validation",
            "E_STORE_VALUE_INVALID",
            "store value nesting exceeds depth ceiling",
        ));
    }
    match value {
        LuaValue::Nil | LuaValue::Bool(_) | LuaValue::Integer(_) => Ok(()),
        LuaValue::Number(n) if n.is_finite() => Ok(()),
        LuaValue::Number(_) => Err(BridgeError::new(
            "validation",
            "E_STORE_VALUE_INVALID",
            "store value must be finite",
        )),
        LuaValue::String(s) if std::str::from_utf8(s.as_bytes()).is_ok() => Ok(()),
        LuaValue::String(_) => Err(BridgeError::new(
            "validation",
            "E_STORE_VALUE_INVALID",
            "store value must be valid UTF-8",
        )),
        LuaValue::Table(pairs) => {
            for (key, child) in pairs {
                match key {
                    LuaValue::String(_) | LuaValue::Integer(_) => {}
                    _ => {
                        return Err(BridgeError::new(
                            "validation",
                            "E_STORE_VALUE_INVALID",
                            "store table keys must be strings or numbers",
                        ));
                    }
                }
                validate_json_value(child, depth + 1)?;
            }
            Ok(())
        }
    }
}

/// Encode a bounded value as deterministic compact JSON.
#[must_use]
pub fn encode_json(value: &LuaValue) -> String {
    match value {
        LuaValue::Nil => "null".to_string(),
        LuaValue::Bool(true) => "true".to_string(),
        LuaValue::Bool(false) => "false".to_string(),
        LuaValue::Integer(i) => i.to_string(),
        LuaValue::Number(n) => {
            if *n == n.trunc() && n.is_finite() && n.abs() < 9.007_199_254_740_992e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        LuaValue::String(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        LuaValue::Table(pairs) => {
            let only_array = pairs
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, LuaValue::Integer(n) if *n == i as i64 + 1));
            let mut out = String::from("[");
            if only_array {
                for (index, (_, child)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&encode_json(child));
                }
                out.push(']');
                return out;
            }
            out = String::from("{");
            for (index, (key, child)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                let key_string = match key {
                    LuaValue::String(s) => s.clone(),
                    LuaValue::Integer(i) => i.to_string(),
                    _ => String::new(),
                };
                out.push_str(&encode_json(&LuaValue::String(key_string)));
                out.push(':');
                out.push_str(&encode_json(child));
            }
            out.push('}');
            out
        }
    }
}

/// Parse the compact JSON subset written by [`encode_json`].
///
/// # Errors
///
/// Returns a bounded message on malformed or over-deep input.
pub fn parse_json(input: &str) -> Result<LuaValue, String> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
        depth: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err("trailing data after JSON value".to_string());
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl JsonParser<'_> {
    fn parse_value(&mut self) -> Result<LuaValue, String> {
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return Err("JSON nesting too deep".to_string());
        }
        self.skip_ws();
        let byte = *self.bytes.get(self.pos).ok_or("unexpected end of JSON")?;
        let value = match byte {
            b'{' => self.parse_object()?,
            b'[' => self.parse_array()?,
            b'"' => LuaValue::String(self.parse_string()?),
            b't' => {
                self.expect_literal("true")?;
                LuaValue::Bool(true)
            }
            b'f' => {
                self.expect_literal("false")?;
                LuaValue::Bool(false)
            }
            b'n' => {
                self.expect_literal("null")?;
                LuaValue::Nil
            }
            _ => self.parse_number()?,
        };
        self.depth -= 1;
        Ok(value)
    }

    fn parse_object(&mut self) -> Result<LuaValue, String> {
        self.pos += 1;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(LuaValue::Table(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err("expected ':' in JSON object".to_string());
            }
            self.pos += 1;
            let value = self.parse_value()?;
            pairs.push((LuaValue::String(key), value));
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or '}' in JSON object".to_string()),
            }
        }
        Ok(LuaValue::Table(pairs))
    }

    fn parse_array(&mut self) -> Result<LuaValue, String> {
        self.pos += 1;
        let mut values = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(LuaValue::array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or ']' in JSON array".to_string()),
            }
        }
        Ok(LuaValue::array(values))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.pos) != Some(&b'"') {
            return Err("expected JSON string".to_string());
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let byte = *self.bytes.get(self.pos).ok_or("unterminated JSON string")?;
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = *self.bytes.get(self.pos).ok_or("unterminated JSON escape")?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.pos..self.pos + 4)
                                .ok_or("bad unicode escape")?;
                            let text =
                                std::str::from_utf8(hex).map_err(|_| "bad unicode escape")?;
                            let code =
                                u32::from_str_radix(text, 16).map_err(|_| "bad unicode escape")?;
                            self.pos += 4;
                            let ch = char::from_u32(code).ok_or("bad unicode scalar")?;
                            out.push(ch);
                        }
                        _ => return Err("unknown JSON escape".to_string()),
                    }
                }
                b if b < 0x20 => return Err("control character in JSON string".to_string()),
                _ => {
                    // Re-decode the full UTF-8 sequence.
                    let start = self.pos - 1;
                    let width = utf8_width(byte);
                    let end = start + width;
                    if end > self.bytes.len() {
                        return Err("truncated UTF-8 in JSON string".to_string());
                    }
                    let text = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| "invalid UTF-8 in JSON string")?;
                    out.push_str(text);
                    self.pos = end;
                }
            }
        }
        Ok(out)
    }

    fn parse_number(&mut self) -> Result<LuaValue, String> {
        let start = self.pos;
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "bad number")?;
        if text.is_empty() {
            return Err("expected JSON value".to_string());
        }
        if !text.contains(['.', 'e', 'E']) {
            if let Ok(integer) = text.parse::<i64>() {
                return Ok(LuaValue::Integer(integer));
            }
        }
        let number = text.parse::<f64>().map_err(|_| "bad number")?;
        Ok(LuaValue::Number(number))
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(format!("expected JSON literal '{literal}'"))
        }
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.bytes.get(self.pos) {
            if byte.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`KvCommitBackend`] stub: proves seam discipline (ordering,
    /// fail-closed intactness) without duplicating commit mechanics. Byte
    /// parity rides the real backend in the wiring crate.
    #[derive(Debug, Default)]
    struct StubBackend {
        state: Mutex<StubState>,
    }

    #[derive(Debug, Default)]
    struct StubState {
        files: HashMap<PathBuf, Vec<u8>>,
        fail_commits: bool,
        commits: u64,
    }

    impl StubBackend {
        fn stored(&self, path: &Path) -> Option<Vec<u8>> {
            self.state
                .lock()
                .expect("stub lock")
                .files
                .get(path)
                .cloned()
        }

        fn commits(&self) -> u64 {
            self.state.lock().expect("stub lock").commits
        }

        /// Test-only seeding helper: writes raw bytes as the stored image.
        fn seed(&self, path: &Path, bytes: Vec<u8>) {
            self.state
                .lock()
                .expect("stub lock")
                .files
                .insert(path.to_path_buf(), bytes);
        }
    }

    impl KvCommitBackend for StubBackend {
        fn commit_store_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), KvCommitError> {
            let mut state = self.state.lock().expect("stub lock");
            if state.fail_commits {
                return Err(KvCommitError::new("stub commit refused"));
            }
            state.commits += 1;
            state.files.insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }

        fn load_store_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, KvCommitError> {
            Ok(self
                .state
                .lock()
                .expect("stub lock")
                .files
                .get(path)
                .cloned())
        }
    }

    fn stub() -> Arc<StubBackend> {
        Arc::new(StubBackend::default())
    }

    fn with_stub(path: PathBuf, backend: &Arc<StubBackend>) -> PluginStore {
        PluginStore::with_backend(
            Some(path),
            Some(backend.clone() as Arc<dyn KvCommitBackend>),
        )
    }

    fn load_with_stub(path: PathBuf, backend: &Arc<StubBackend>) -> Result<PluginStore, String> {
        PluginStore::load_with_backend(path, Some(backend.clone() as Arc<dyn KvCommitBackend>))
    }

    #[test]
    fn in_memory_store_operations() {
        let mut store = PluginStore::in_memory();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.path(), None);

        store
            .set("alpha", LuaValue::String("val1".to_string()))
            .expect("set alpha");
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get("alpha"),
            Some(LuaValue::String("val1".to_string()))
        );

        // Deletion via Nil
        store.set("alpha", LuaValue::Nil).expect("delete alpha");
        assert!(store.is_empty());
        assert_eq!(store.get("alpha"), None);
    }

    #[test]
    fn backend_absent_path_store_fails_closed() {
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_backend(Some(path.clone()), None);

        // Writes fail closed with E_STORE_IO; nothing is published.
        let err = store
            .set("alpha", LuaValue::String("val1".to_string()))
            .expect_err("absent backend must fail");
        assert_eq!(err.code, "E_STORE_IO");
        assert!(store.is_empty());

        // Loads start clean and empty.
        let loaded =
            PluginStore::load_with_backend(path, None).expect("absent backend loads empty");
        assert!(loaded.is_empty());
    }

    #[test]
    fn candidate_persisted_before_in_memory_publish_on_rejected_write() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        // First write succeeds
        store
            .set("theme", LuaValue::String("light".to_string()))
            .expect("first write succeeds");
        assert_eq!(
            store.get("theme"),
            Some(LuaValue::String("light".to_string()))
        );

        // Inject write failure
        backend.state.lock().expect("stub lock").fail_commits = true;
        let err = store.set("theme", LuaValue::String("dark".to_string()));
        assert!(err.is_err());

        // In-memory state remains intact with previous value (not candidate "dark")
        assert_eq!(
            store.get("theme"),
            Some(LuaValue::String("light".to_string()))
        );

        // Persisted state in the backend remains "light"
        let persisted =
            String::from_utf8(backend.stored(&path).expect("read store")).expect("store is UTF-8");
        assert!(persisted.contains("light"));
        assert!(!persisted.contains("dark"));
    }

    #[test]
    fn store_preserves_committed_settings_on_rejected_replacement() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        store
            .set("setting.a", LuaValue::Integer(42))
            .expect("initial commit");
        assert_eq!(store.get("setting.a"), Some(LuaValue::Integer(42)));
        let committed = backend.commits();

        // Inject replacement failure
        backend.state.lock().expect("stub lock").fail_commits = true;

        let err = store.set("setting.a", LuaValue::Integer(100));
        assert!(err.is_err(), "replacement failure must fail closed");
        assert_eq!(err.unwrap_err().code, "E_STORE_IO");

        // In-memory state was NOT mutated to candidate
        assert_eq!(store.get("setting.a"), Some(LuaValue::Integer(42)));

        // Committed image was NOT replaced and still contains the prior setting
        assert_eq!(backend.commits(), committed);
        let disk_text =
            String::from_utf8(backend.stored(&path).expect("read store")).expect("store is UTF-8");
        assert!(disk_text.contains("42"));
        assert!(!disk_text.contains("100"));
    }

    #[test]
    fn store_preserves_committed_settings_on_rejected_deletion_transaction() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        store
            .set("key1", LuaValue::String("preserved".to_string()))
            .expect("initial set");
        assert_eq!(
            store.get("key1"),
            Some(LuaValue::String("preserved".to_string()))
        );

        // Fail replacement during deletion
        backend.state.lock().expect("stub lock").fail_commits = true;
        let err = store.set("key1", LuaValue::Nil);
        assert!(err.is_err());

        // In-memory key must NOT be deleted
        assert_eq!(
            store.get("key1"),
            Some(LuaValue::String("preserved".to_string()))
        );

        // Committed image must still contain the key
        let text =
            String::from_utf8(backend.stored(&path).expect("read store")).expect("store is UTF-8");
        assert!(text.contains("preserved"));

        // Unblock and retry deletion
        backend.state.lock().expect("stub lock").fail_commits = false;
        store.set("key1", LuaValue::Nil).expect("deletion succeeds");
        assert_eq!(store.get("key1"), None);
        let updated =
            String::from_utf8(backend.stored(&path).expect("read store")).expect("store is UTF-8");
        assert!(!updated.contains("preserved"));
    }

    #[test]
    fn failed_commit_leaves_no_committed_image() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        // Inject commit failure before anything is committed.
        backend.state.lock().expect("stub lock").fail_commits = true;
        let err = store.set("alpha", LuaValue::Integer(1));
        assert!(err.is_err());

        // Nothing committed, in-memory empty.
        assert!(backend.stored(&path).is_none());
        assert!(store.is_empty());

        // Restored backend commits normally.
        backend.state.lock().expect("stub lock").fail_commits = false;
        store
            .set("alpha", LuaValue::Integer(1))
            .expect("commit succeeds");
        assert!(backend.stored(&path).is_some());
        assert_eq!(store.get("alpha"), Some(LuaValue::Integer(1)));
    }

    #[test]
    fn stub_backed_store_replacement_and_reload() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        let mut store = with_stub(path.clone(), &backend);
        assert!(store.is_empty());

        store
            .set("session.id", LuaValue::String("s-123".to_string()))
            .expect("first set");
        store
            .set("session.count", LuaValue::Integer(7))
            .expect("second set");

        // Reload from the backend in a fresh store instance
        let reloaded = load_with_stub(path, &backend).expect("reload from backend");
        assert_eq!(
            reloaded.get("session.id"),
            Some(LuaValue::String("s-123".to_string()))
        );
        assert_eq!(reloaded.get("session.count"), Some(LuaValue::Integer(7)));
    }

    #[test]
    fn quota_denial_never_reaches_the_backend() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        // Fill to the entry ceiling through successful commits.
        for i in 0..STORE_MAX_ENTRIES {
            store
                .set(&format!("k{i}"), LuaValue::Integer(i as i64))
                .expect("fill entry");
        }
        let committed = backend.commits();

        // One more entry trips the quota before any backend call.
        let err = store
            .set("overflow", LuaValue::Integer(-1))
            .expect_err("quota must fail");
        assert_eq!(err.code, "E_STORE_QUOTA");
        assert_eq!(
            backend.commits(),
            committed,
            "denied writes never reach the backend"
        );
        assert!(store.get("overflow").is_none());
    }

    #[test]
    fn load_rejects_invalid_keys() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        // Key starting with invalid char
        backend.seed(&path, b"{\"_bad\": 1}".to_vec());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Key with uppercase
        backend.seed(&path, b"{\"BadKey\": 1}".to_vec());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Key with empty dot segments
        backend.seed(&path, b"{\"a..b\": 1}".to_vec());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Oversized key (> 128 bytes)
        let long_key = "a".repeat(129);
        backend.seed(&path, format!("{{\"{long_key}\": 1}}").into_bytes());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));
    }

    #[test]
    fn load_rejects_oversized_value() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        let big_str = "x".repeat(STORE_MAX_VALUE_BYTES + 1);
        backend.seed(&path, format!("{{\"k\": \"{big_str}\"}}").into_bytes());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));
    }

    #[test]
    fn load_rejects_non_finite_and_invalid_values() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        // Non-object root (string)
        backend.seed(&path, b"\"just a string\"".to_vec());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store root must be an object"));

        // Non-object root (array)
        backend.seed(&path, b"[1, 2, 3]".to_vec());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store keys must be strings"));
    }

    #[test]
    fn load_rejects_quota_violations() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        // Too many entries (> STORE_MAX_ENTRIES)
        let mut text = String::from("{");
        for i in 0..=STORE_MAX_ENTRIES {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&format!("\"k{i}\": {i}"));
        }
        text.push('}');
        backend.seed(&path, text.into_bytes());
        let err = load_with_stub(path.clone(), &backend);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store quota violated"));
    }

    #[test]
    fn nesting_depth_enforced_across_set_and_load() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        // Construct 13 levels of nested tables with leaf scalar (leaf depth = 2 + 13 + 1 = 16 <= 16, passes)
        let mut ok_val = LuaValue::Table(vec![(
            LuaValue::String("leaf".to_string()),
            LuaValue::Integer(42),
        )]);
        for i in 0..13 {
            ok_val = LuaValue::Table(vec![(LuaValue::String(format!("lvl{i}")), ok_val)]);
        }
        store
            .set("nested.ok", ok_val)
            .expect("depth 16 must succeed");

        // Construct 14 levels of nested tables with leaf scalar (leaf depth = 2 + 14 + 1 = 17 > 16, fails)
        let mut deep_val = LuaValue::Table(vec![(
            LuaValue::String("leaf".to_string()),
            LuaValue::Integer(42),
        )]);
        for i in 0..14 {
            deep_val = LuaValue::Table(vec![(LuaValue::String(format!("lvl{i}")), deep_val)]);
        }
        let err = store.set("nested.deep", deep_val);
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().code, "E_STORE_VALUE_INVALID");

        // Loading the valid store must succeed
        let loaded = load_with_stub(path.clone(), &backend).expect("valid store loads");
        assert!(loaded.get("nested.ok").is_some());
    }

    #[test]
    fn load_failure_preserves_last_good_state() {
        let backend = stub();
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = with_stub(path.clone(), &backend);

        store
            .set("good.key", LuaValue::String("good.val".to_string()))
            .expect("initial set");
        assert_eq!(
            store.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );

        // Verify valid load
        let loaded = load_with_stub(path.clone(), &backend).expect("load valid");
        assert_eq!(
            loaded.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );

        // Corrupt the image with an invalid key
        backend.seed(&path, b"{\"INVALID_KEY\": 123}".to_vec());
        let failed_load = load_with_stub(path.clone(), &backend);
        assert!(failed_load.is_err());

        // In-memory `store` was unaffected and still holds good state
        assert_eq!(
            store.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );
    }
}
