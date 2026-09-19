//! Bounded, atomic, quota-enforced plugin store (`bitty.store.*`, RFC Gap C).
//!
//! The store is plugin-scoped and survives suspension, reload, and generation
//! disposal. It is persisted as JSON under the platform data directory
//! (`$XDG_DATA_HOME/bitty/plugins-state/<plugin-id>/store.json`), written
//! temp-then-rename so a partial write is never observable. The quota and
//! value bounds are enforced before any mutation; overflow fails closed with
//! `E_STORE_QUOTA` and there is no eviction and no partial write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_lua::{BridgeError, LuaValue};

use super::fs::{FileSystem, NativeFileSystem, write_atomic_durably};

static STORE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    fs: Arc<dyn FileSystem>,
}

impl PluginStore {
    /// Create an in-memory store with no persistence path.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: BTreeMap::new(),
            fs: Arc::new(NativeFileSystem),
        }
    }

    /// Create an empty store configured with an explicit path and filesystem adapter.
    #[must_use]
    pub fn with_filesystem(path: Option<PathBuf>, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            path,
            entries: BTreeMap::new(),
            fs,
        }
    }

    /// Load a store from `path`, or start empty when the file is absent.
    ///
    /// # Errors
    ///
    /// Returns a bounded message when the file exists but is unreadable,
    /// over the file ceiling, or not the JSON subset this module writes.
    pub fn load(path: PathBuf) -> Result<Self, String> {
        Self::load_with_fs(path, Arc::new(NativeFileSystem))
    }

    /// Load a store from `path` using the specified filesystem adapter.
    ///
    /// # Errors
    ///
    /// Returns a bounded message when the file exists but is unreadable,
    /// over the file ceiling, or not the JSON subset this module writes.
    pub fn load_with_fs(path: PathBuf, fs: Arc<dyn FileSystem>) -> Result<Self, String> {
        if !fs.exists(&path) {
            return Ok(Self {
                path: Some(path),
                entries: BTreeMap::new(),
                fs,
            });
        }
        let len = fs
            .metadata_len(&path)
            .map_err(|e| format!("store metadata: {e}"))?;
        if len as usize > STORE_FILE_MAX_BYTES {
            return Err("plugin store exceeds the file ceiling".to_string());
        }
        let text = fs
            .read_to_string(&path)
            .map_err(|e| format!("store read: {e}"))?;
        let value = parse_json(&text).map_err(|e| format!("store parse: {e}"))?;
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
            fs,
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
        let Some(parent) = path.parent() else {
            return Err(BridgeError::new(
                "runtime",
                "E_STORE_IO",
                "invalid plugin state path",
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
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("store.json");
        let temp = parent.join(format!(
            "{}.tmp-{}-{}",
            file_name,
            std::process::id(),
            STORE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        write_atomic_durably(&*self.fs, path, buffer.as_bytes(), &temp).map_err(|_| {
            BridgeError::new(
                "runtime",
                "E_STORE_IO",
                "could not commit the plugin state file",
            )
        })
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
    use crate::plugin_runtime::fs::FakeFileSystem;

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
    fn candidate_persisted_before_in_memory_publish_on_rejected_write() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

        // First write succeeds
        store
            .set("theme", LuaValue::String("light".to_string()))
            .expect("first write succeeds");
        assert_eq!(
            store.get("theme"),
            Some(LuaValue::String("light".to_string()))
        );

        // Inject write failure
        fake_fs.set_fail_writes(true);
        let err = store.set("theme", LuaValue::String("dark".to_string()));
        assert!(err.is_err());

        // In-memory state remains intact with previous value (not candidate "dark")
        assert_eq!(
            store.get("theme"),
            Some(LuaValue::String("light".to_string()))
        );

        // Persisted state in fake fs remains "light"
        let persisted = fake_fs.read_to_string(&path).expect("read store");
        assert!(persisted.contains("light"));
        assert!(!persisted.contains("dark"));
    }

    #[test]
    fn store_preserves_committed_settings_on_rejected_replacement() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

        store
            .set("setting.a", LuaValue::Integer(42))
            .expect("initial commit");
        assert_eq!(store.get("setting.a"), Some(LuaValue::Integer(42)));

        // Inject rename/replacement failure
        fake_fs.set_fail_renames(true);

        let err = store.set("setting.a", LuaValue::Integer(100));
        assert!(err.is_err(), "replacement failure must fail closed");

        // In-memory state was NOT mutated to candidate
        assert_eq!(store.get("setting.a"), Some(LuaValue::Integer(42)));

        // Destination was NOT deleted and still contains prior setting
        assert!(fake_fs.exists(&path));
        let disk_text = fake_fs.read_to_string(&path).expect("read store");
        assert!(disk_text.contains("42"));
        assert!(!disk_text.contains("100"));

        // Temporary file was cleaned up
        let temp_exists = fake_fs
            .get_file("/plugins-state/my-plugin/store.json.tmp")
            .is_some();
        assert!(!temp_exists);
    }

    #[test]
    fn store_preserves_committed_settings_on_rejected_deletion_transaction() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

        store
            .set("key1", LuaValue::String("preserved".to_string()))
            .expect("initial set");
        assert_eq!(
            store.get("key1"),
            Some(LuaValue::String("preserved".to_string()))
        );

        // Fail replacement during deletion
        fake_fs.set_fail_renames(true);
        let err = store.set("key1", LuaValue::Nil);
        assert!(err.is_err());

        // In-memory key must NOT be deleted
        assert_eq!(
            store.get("key1"),
            Some(LuaValue::String("preserved".to_string()))
        );

        // On-disk file must still contain the key
        let text = fake_fs.read_to_string(&path).expect("read store");
        assert!(text.contains("preserved"));

        // Unblock and retry deletion
        fake_fs.set_fail_renames(false);
        store.set("key1", LuaValue::Nil).expect("deletion succeeds");
        assert_eq!(store.get("key1"), None);
        let updated = fake_fs.read_to_string(&path).expect("read store");
        assert!(!updated.contains("preserved"));
    }

    #[test]
    fn distinguish_atomicity_from_durability() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

        // Inject durability (sync) failure
        fake_fs.set_fail_syncs(true);
        let err = store.set("alpha", LuaValue::Integer(1));
        assert!(err.is_err());

        // Atomicity preserved: destination not created, in-memory empty
        assert!(!fake_fs.exists(&path));
        assert!(store.is_empty());

        // Restore durability and check order
        fake_fs.set_fail_syncs(false);
        store
            .set("alpha", LuaValue::Integer(1))
            .expect("sync and rename succeed");
        assert!(fake_fs.exists(&path));
        assert_eq!(store.get("alpha"), Some(LuaValue::Integer(1)));
    }

    #[test]
    fn native_fs_atomic_store_replacement_and_reload() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-test-native-store-{}-{}",
            std::process::id(),
            STORE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("store.json");

        let mut store = PluginStore::load(path.clone()).expect("load empty");
        assert!(store.is_empty());

        store
            .set("session.id", LuaValue::String("s-123".to_string()))
            .expect("first set");
        store
            .set("session.count", LuaValue::Integer(7))
            .expect("second set");

        // Reload from disk in a fresh store instance
        let reloaded = PluginStore::load(path.clone()).expect("reload from disk");
        assert_eq!(
            reloaded.get("session.id"),
            Some(LuaValue::String("s-123".to_string()))
        );
        assert_eq!(reloaded.get("session.count"), Some(LuaValue::Integer(7)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_invalid_keys() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        // Key starting with invalid char
        fake_fs.set_file(path.clone(), b"{\"_bad\": 1}".to_vec());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Key with uppercase
        fake_fs.set_file(path.clone(), b"{\"BadKey\": 1}".to_vec());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Key with empty dot segments
        fake_fs.set_file(path.clone(), b"{\"a..b\": 1}".to_vec());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));

        // Oversized key (> 128 bytes)
        let long_key = "a".repeat(129);
        fake_fs.set_file(path.clone(), format!("{{\"{long_key}\": 1}}").into_bytes());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));
    }

    #[test]
    fn load_rejects_oversized_value() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        let big_str = "x".repeat(STORE_MAX_VALUE_BYTES + 1);
        fake_fs.set_file(
            path.clone(),
            format!("{{\"k\": \"{big_str}\"}}").into_bytes(),
        );
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("violates invariants"));
    }

    #[test]
    fn load_rejects_non_finite_and_invalid_values() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");

        // Non-object root (string)
        fake_fs.set_file(path.clone(), b"\"just a string\"".to_vec());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store root must be an object"));

        // Non-object root (array)
        fake_fs.set_file(path.clone(), b"[1, 2, 3]".to_vec());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store keys must be strings"));
    }

    #[test]
    fn load_rejects_quota_violations() {
        let fake_fs = Arc::new(FakeFileSystem::new());
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
        fake_fs.set_file(path.clone(), text.into_bytes());
        let err = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("store quota violated"));
    }

    #[test]
    fn nesting_depth_enforced_across_set_and_load() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

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
        let loaded =
            PluginStore::load_with_fs(path.clone(), fake_fs.clone()).expect("valid store loads");
        assert!(loaded.get("nested.ok").is_some());
    }

    #[test]
    fn load_failure_preserves_last_good_state() {
        let fake_fs = Arc::new(FakeFileSystem::new());
        let path = PathBuf::from("/plugins-state/my-plugin/store.json");
        let mut store = PluginStore::with_filesystem(Some(path.clone()), fake_fs.clone());

        store
            .set("good.key", LuaValue::String("good.val".to_string()))
            .expect("initial set");
        assert_eq!(
            store.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );

        // Verify valid load
        let loaded = PluginStore::load_with_fs(path.clone(), fake_fs.clone()).expect("load valid");
        assert_eq!(
            loaded.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );

        // Corrupt on disk with invalid key
        fake_fs.set_file(path.clone(), b"{\"INVALID_KEY\": 123}".to_vec());
        let failed_load = PluginStore::load_with_fs(path.clone(), fake_fs.clone());
        assert!(failed_load.is_err());

        // In-memory `store` was unaffected and still holds good state
        assert_eq!(
            store.get("good.key"),
            Some(LuaValue::String("good.val".to_string()))
        );
    }
}
