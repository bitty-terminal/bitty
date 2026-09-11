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

use bitty_lua::{BridgeError, LuaValue};

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

/// One plugin's bounded key/value store.
#[derive(Debug)]
pub struct PluginStore {
    path: Option<PathBuf>,
    entries: BTreeMap<String, LuaValue>,
}

impl PluginStore {
    /// Create an in-memory store with no persistence path.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: BTreeMap::new(),
        }
    }

    /// Load a store from `path`, or start empty when the file is absent.
    ///
    /// # Errors
    ///
    /// Returns a bounded message when the file exists but is unreadable,
    /// over the file ceiling, or not the JSON subset this module writes.
    pub fn load(path: PathBuf) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                path: Some(path),
                entries: BTreeMap::new(),
            });
        }
        let metadata = std::fs::metadata(&path).map_err(|e| format!("store metadata: {e}"))?;
        if metadata.len() as usize > STORE_FILE_MAX_BYTES {
            return Err("plugin store exceeds the file ceiling".to_string());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("store read: {e}"))?;
        let value = parse_json(&text).map_err(|e| format!("store parse: {e}"))?;
        let mut entries = BTreeMap::new();
        if let LuaValue::Table(pairs) = value {
            for (key, entry) in pairs {
                let LuaValue::String(key) = key else {
                    return Err("store keys must be strings".to_string());
                };
                entries.insert(key, entry);
            }
        } else {
            return Err("store root must be an object".to_string());
        }
        Ok(Self {
            path: Some(path),
            entries,
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
    pub fn set(&mut self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        validate_key(key)?;
        if matches!(value, LuaValue::Nil) {
            self.entries.remove(key);
            return self.persist();
        }
        let encoded = encode_json(&value);
        if encoded.len() > STORE_MAX_VALUE_BYTES {
            return Err(BridgeError::new(
                "validation",
                "E_STORE_VALUE_INVALID",
                "store value exceeds the 8 KiB ceiling",
            ));
        }
        validate_json_value(&value)?;

        let mut candidate = self.entries.clone();
        candidate.insert(key.to_string(), value);
        let total: usize = candidate
            .iter()
            .map(|(k, v)| k.len() + encode_json(v).len())
            .sum();
        if candidate.len() > STORE_MAX_ENTRIES || total > STORE_MAX_TOTAL_BYTES {
            return Err(BridgeError::new(
                "budget",
                "E_STORE_QUOTA",
                "plugin store quota exceeded",
            ));
        }
        self.entries = candidate;
        self.persist()
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

    fn persist(&self) -> Result<(), BridgeError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| {
                BridgeError::new(
                    "runtime",
                    "E_STORE_IO",
                    "could not create the plugin state directory",
                )
            })?;
        }
        let mut buffer = String::from("{");
        for (index, (key, value)) in self.entries.iter().enumerate() {
            if index > 0 {
                buffer.push(',');
            }
            buffer.push_str(&encode_json(&LuaValue::String(key.clone())));
            buffer.push(':');
            buffer.push_str(&encode_json(value));
        }
        buffer.push('}');
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, buffer.as_bytes()).map_err(|_| {
            BridgeError::new(
                "runtime",
                "E_STORE_IO",
                "could not write the plugin state file",
            )
        })?;
        match std::fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Windows cannot rename over an existing file; the fallback is
                // still bounded and never leaves the temp file as the store.
                let _ = std::fs::remove_file(path);
                std::fs::rename(&temp, path).map_err(|_| {
                    BridgeError::new(
                        "runtime",
                        "E_STORE_IO",
                        "could not commit the plugin state file",
                    )
                })
            }
        }
    }
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

fn validate_json_value(value: &LuaValue) -> Result<(), BridgeError> {
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
                validate_json_value(child)?;
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

const JSON_MAX_DEPTH: usize = 16;

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
