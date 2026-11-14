//! Application backend for the `bitty.wheel` / `bitty.config.wheel` management
//! surface under `config.manage` (CTX-1817, bitty#3143).
//!
//! Owns:
//! - Owner-only (`0700` dir, `0600` file, symlink/foreign-UID refusal) atomic
//!   reads and writes for `$XDG_DATA_HOME/wheel/{settings.json,credentials.json,approvals.json}`;
//! - Non-evaluating inspection of `$XDG_CONFIG_HOME/wheel` (`init.lua`,
//!   `providers/<name>.lua`) and boolean environment variable probes (`env` >
//!   `global_config` > `data_credentials` > `none`) so secret values never
//!   cross the Lua bridge;
//! - Self-contained bounded RFC 8259 JSON parser and canonical serializer
//!   (<= 64 KiB, depth <= 16) with zero external crate dependencies.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_runtime::plugin_runtime::{
    ConfigApplyOutcome, ConfigReadFailure, ConfigWriteFailure, WheelApprovalRow,
    WheelMutationRendering, WheelPermissionRow, WheelProviderRow, WheelSettingValue, WheelSnapshot,
    candidate_digest,
};

/// Maximum bytes accepted for any Wheel JSON or Lua config file.
pub(crate) const WHEEL_MAX_FILE_BYTES: usize = 64 * 1024;

/// Maximum JSON nesting depth.
const WHEEL_JSON_MAX_DEPTH: usize = 16;

/// Maximum entries retained in `approvals.json`.
const WHEEL_APPROVALS_MAX_ENTRIES: usize = 32;

/// Owner-only file mode on Unix (`0600`).
#[cfg(unix)]
const WHEEL_FILE_MODE: u32 = 0o600;

/// Owner-only directory mode on Unix (`0700`).
#[cfg(unix)]
const WHEEL_DIR_MODE: u32 = 0o700;

/// Temp file prefix for atomic writes under `$XDG_DATA_HOME/wheel`.
const WHEEL_TEMP_PREFIX: &str = ".bitty-wheel-";

/// Lock directory name used by Wheel CLI and Core for serialized store updates.
const WHEEL_LOCK_DIR_NAME: &str = ".approvals.lock";

/// Maximum lock acquisition attempts before failing closed (`40 * 25ms = 1s`).
const WHEEL_LOCK_MAX_ATTEMPTS: usize = 40;

/// Attempt threshold at which a stale lock's recorded owner PID is checked.
const WHEEL_LOCK_RECLAIM_ATTEMPT: usize = 10;

/// Backoff sleep in milliseconds between lock acquisition attempts.
const WHEEL_LOCK_SLEEP_MS: u64 = 25;

/// Monotonic counter so concurrent temp files in one process never collide.
static WHEEL_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Built-in provider catalog descriptor matching `wheel/lua/wheel/config.lua`.
pub(crate) struct BuiltinProviderSpec {
    pub(crate) name: &'static str,
    pub(crate) label: &'static str,
    pub(crate) protocol: &'static str,
    pub(crate) default_model: &'static str,
    pub(crate) auth_env: &'static str,
    pub(crate) base_url: &'static str,
}

/// Canonical built-in provider catalog in deterministic display order.
pub(crate) const BUILTIN_PROVIDERS: &[BuiltinProviderSpec] = &[
    BuiltinProviderSpec {
        name: "opencode-go",
        label: "OpenCode Go",
        protocol: "openai_compat",
        default_model: "glm-5.1",
        auth_env: "OPENCODE_API_KEY",
        base_url: "https://opencode.ai/zen/go/v1",
    },
    BuiltinProviderSpec {
        name: "openai",
        label: "OpenAI",
        protocol: "openai_compat",
        default_model: "gpt-5.4",
        auth_env: "OPENAI_API_KEY",
        base_url: "https://api.openai.com/v1",
    },
    BuiltinProviderSpec {
        name: "anthropic",
        label: "Anthropic",
        protocol: "anthropic_messages",
        default_model: "claude-sonnet-4-6",
        auth_env: "ANTHROPIC_API_KEY",
        base_url: "https://api.anthropic.com/v1",
    },
    BuiltinProviderSpec {
        name: "deepseek",
        label: "DeepSeek",
        protocol: "openai_compat",
        default_model: "deepseek-v4",
        auth_env: "DEEPSEEK_API_KEY",
        base_url: "https://api.deepseek.com/v1",
    },
    BuiltinProviderSpec {
        name: "openrouter",
        label: "OpenRouter",
        protocol: "openai_compat",
        default_model: "moonshotai/kimi-k2.5",
        auth_env: "OPENROUTER_API_KEY",
        base_url: "https://openrouter.ai/api/v1",
    },
    BuiltinProviderSpec {
        name: "ollama",
        label: "Ollama (Local)",
        protocol: "openai_compat",
        default_model: "qwen3-coder",
        auth_env: "",
        base_url: "http://127.0.0.1:11434/v1",
    },
];

/// Canonical built-in tool permissions and their default modes.
pub(crate) const BUILTIN_PERMISSIONS: &[(&str, &str)] = &[
    ("read_file", "auto"),
    ("write_file", "ask"),
    ("edit_file", "ask"),
    ("glob", "auto"),
    ("grep", "auto"),
    ("bash", "ask"),
];

/// All recognized Wheel permission tool keys (including dashboard alias keys).
const RECOGNIZED_PERMISSION_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "bash",
    "fs_read",
    "fs_write",
    "run_command",
    "git",
    "network",
];

/// Minimal, bounded RFC 8259 JSON value representation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    pub(crate) fn as_object(&self) -> Option<&BTreeMap<String, JsonValue>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    pub(crate) fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, JsonValue>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            Self::Float(f) if f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }
}

/// Parse one bounded JSON document (`<= 64 KiB`, depth `<= 16`).
pub(crate) fn parse_json(input: &str) -> Result<JsonValue, ()> {
    if input.len() > WHEEL_MAX_FILE_BYTES {
        return Err(());
    }
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(());
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, ()> {
        if depth > WHEEL_JSON_MAX_DEPTH {
            return Err(());
        }
        self.skip_ws();
        let b = self.peek().ok_or(())?;
        match b {
            b'n' => self.parse_null(),
            b't' | b'f' => self.parse_bool(),
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(()),
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, ()> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(())
        }
    }

    fn parse_bool(&mut self) -> Result<JsonValue, ()> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(())
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        if self.peek() != Some(b'"') {
            return Err(());
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = *self.bytes.get(self.pos).ok_or(())?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self.bytes.get(self.pos..self.pos + 4).ok_or(())?;
                            let hex_str = std::str::from_utf8(hex).map_err(|_| ())?;
                            let code = u16::from_str_radix(hex_str, 16).map_err(|_| ())?;
                            self.pos += 4;
                            let scalar = if (0xD800..=0xDBFF).contains(&code) {
                                if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                    return Err(());
                                }
                                self.pos += 2;
                                let low_hex = self.bytes.get(self.pos..self.pos + 4).ok_or(())?;
                                let low_str = std::str::from_utf8(low_hex).map_err(|_| ())?;
                                let low = u16::from_str_radix(low_str, 16).map_err(|_| ())?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(());
                                }
                                self.pos += 4;
                                0x10000
                                    + (((u32::from(code) - 0xD800) << 10)
                                        | (u32::from(low) - 0xDC00))
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                return Err(());
                            } else {
                                u32::from(code)
                            };
                            let ch = char::from_u32(scalar).ok_or(())?;
                            out.push(ch);
                        }
                        _ => return Err(()),
                    }
                }
                0x00..=0x1F => return Err(()),
                _ => {
                    let start = self.pos - 1;
                    let width = utf8_char_width(b).ok_or(())?;
                    let end = start + width;
                    if end > self.bytes.len() {
                        return Err(());
                    }
                    let slice = std::str::from_utf8(&self.bytes[start..end]).map_err(|_| ())?;
                    out.push_str(slice);
                    self.pos = end;
                }
            }
        }
        Err(())
    }

    fn parse_number(&mut self) -> Result<JsonValue, ()> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(()),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| ())?;
        if is_float {
            let f: f64 = s.parse().map_err(|_| ())?;
            if !f.is_finite() {
                return Err(());
            }
            Ok(JsonValue::Float(f))
        } else {
            let n: i64 = s.parse().map_err(|_| ())?;
            Ok(JsonValue::Int(n))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, ()> {
        self.pos += 1; // '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, ()> {
        self.pos += 1; // '{'
        self.skip_ws();
        let mut map = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.pos += 1;
            let val = self.parse_value(depth)?;
            map.insert(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(map));
                }
                _ => return Err(()),
            }
        }
    }
}

const fn utf8_char_width(first: u8) -> Option<usize> {
    if first < 0x80 {
        Some(1)
    } else if (first & 0xE0) == 0xC0 {
        Some(2)
    } else if (first & 0xF0) == 0xE0 {
        Some(3)
    } else if (first & 0xF8) == 0xF0 {
        Some(4)
    } else {
        None
    }
}

/// Serialize a [`JsonValue`] deterministically with trailing newline.
pub(crate) fn serialize_json(value: &JsonValue) -> String {
    let mut out = String::new();
    write_json_value(value, &mut out);
    out.push('\n');
    out
}

fn write_json_value(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Int(n) => out.push_str(&n.to_string()),
        JsonValue::Float(f) => {
            if f.is_finite() {
                let s = f.to_string();
                out.push_str(&s);
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    out.push_str(".0");
                }
            } else {
                out.push_str("null");
            }
        }
        JsonValue::String(s) => write_json_string(s, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_json_value(item, out);
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_json_value(v, out);
            }
            out.push('}');
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Verify an existing Wheel private directory is not a symlink, is a directory,
/// is owned by the current effective UID, and has no group/other mode bits.
fn check_existing_private_dir(dir: &Path) -> Result<bool, ConfigWriteFailure> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(ConfigWriteFailure::Io),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(ConfigWriteFailure::Unsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(ConfigWriteFailure::Unsafe);
        }
        if (meta.permissions().mode() & 0o077) != 0 {
            return Err(ConfigWriteFailure::Unsafe);
        }
    }
    Ok(true)
}

/// Ensure the Wheel private data directory exists with `0700` permissions.
fn ensure_private_dir(dir: &Path) -> Result<(), ConfigWriteFailure> {
    if check_existing_private_dir(dir)? {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(WHEEL_DIR_MODE);
        builder.create(dir).map_err(|_| ConfigWriteFailure::Io)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(WHEEL_DIR_MODE))
            .map_err(|_| ConfigWriteFailure::Io)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|_| ConfigWriteFailure::Io)?;
    }
    if !check_existing_private_dir(dir)? {
        return Err(ConfigWriteFailure::Io);
    }
    Ok(())
}

type JsonStoreFile = Option<(String, BTreeMap<String, JsonValue>)>;

/// Read at most `WHEEL_MAX_FILE_BYTES + 1` bytes from one opened file handle and
/// reject any input exceeding `WHEEL_MAX_FILE_BYTES`.
fn read_bounded_utf8_file(path: &Path) -> Result<String, ConfigWriteFailure> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|_| ConfigWriteFailure::Io)?;
    let mut buf = Vec::new();
    file.take((WHEEL_MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|_| ConfigWriteFailure::Io)?;
    if buf.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    String::from_utf8(buf).map_err(|_| ConfigWriteFailure::Invalid)
}

/// Read one private JSON store file (`settings.json`, `credentials.json`, or `approvals.json`).
/// Returns `Ok(None)` when either the directory or the file does not exist yet.
fn read_private_json_for_write(
    dir: &Path,
    filename: &str,
) -> Result<JsonStoreFile, ConfigWriteFailure> {
    if !check_existing_private_dir(dir)? {
        return Ok(None);
    }
    let path = dir.join(filename);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ConfigWriteFailure::Io),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(ConfigWriteFailure::Unsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(ConfigWriteFailure::Unsafe);
        }
        if (meta.permissions().mode() & 0o077) != 0 {
            return Err(ConfigWriteFailure::Unsafe);
        }
    }
    if meta.len() > WHEEL_MAX_FILE_BYTES as u64 {
        return Err(ConfigWriteFailure::Invalid);
    }
    let raw = read_bounded_utf8_file(&path)?;
    let parsed = parse_json(&raw).map_err(|()| ConfigWriteFailure::Invalid)?;
    let JsonValue::Object(map) = parsed else {
        return Err(ConfigWriteFailure::Invalid);
    };
    Ok(Some((raw, map)))
}

fn map_write_to_read_failure(failure: ConfigWriteFailure) -> ConfigReadFailure {
    match failure {
        ConfigWriteFailure::Invalid => ConfigReadFailure::Invalid,
        ConfigWriteFailure::Unavailable => ConfigReadFailure::Unavailable,
        ConfigWriteFailure::Unsafe => ConfigReadFailure::Unsafe,
        ConfigWriteFailure::Stale | ConfigWriteFailure::Io => ConfigReadFailure::Io,
    }
}

/// Read the recorded PID from `.approvals.lock/owner*` if present and valid.
fn read_lock_owner_pid(owner_file: &Path) -> Option<u32> {
    let raw = read_bounded_utf8_file(owner_file).ok()?;
    let parsed: u32 = raw.trim().parse().ok()?;
    (parsed >= 1).then_some(parsed)
}

/// Check whether `pid` is still alive (`false` only when definitively dead).
fn is_lock_owner_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return true;
        };
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw_pid), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true,
            Err(nix::errno::Errno::ESRCH) => false,
            Err(_) => true,
        }
    }
    #[cfg(windows)]
    {
        bitty_winjob::process_is_running(pid).unwrap_or(true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

fn create_private_lock_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(WHEEL_DIR_MODE);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

fn remove_lock_dir_contents(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.take(16).flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Create a private staging directory and populate its unique `owner.<token>` file
/// before the lock directory is ever published at `.approvals.lock`.
///
/// Both Bitty Core (`wheel_manage.rs`) and the Wheel CLI (`lua/wheel/config.lua`)
/// use this staged publication protocol so neither writer ever creates an empty
/// `.approvals.lock` directory prior to owner publication.
fn prepare_staged_lock_dir(
    stage_dir: &Path,
    owner_filename: &str,
    pid: u32,
) -> std::io::Result<()> {
    create_private_lock_dir(stage_dir)?;
    let staged_owner = stage_dir.join(owner_filename);
    let write_res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(WHEEL_FILE_MODE)
                .open(&staged_owner)?;
            file.write_all(format!("{pid}\n").as_bytes())?;
            file.sync_all()
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&staged_owner, format!("{pid}\n"))
        }
    })();
    if write_res.is_err() {
        remove_lock_dir_contents(stage_dir);
    }
    write_res
}

/// Atomically publish a populated `stage_dir` to `lock_dir`.
///
/// Refuses to overwrite an existing `lock_dir` path (even if empty), and on
/// concurrent `rename` attempts into a missing `lock_dir`, the OS kernel's
/// directory rename guarantees that once the winner's non-empty `stage_dir`
/// lands at `lock_dir`, every concurrent contender's `rename` fails with
/// `ENOTEMPTY`/`EEXIST`.
fn publish_staged_lock_dir(stage_dir: &Path, lock_dir: &Path) -> bool {
    if std::fs::symlink_metadata(lock_dir).is_ok() {
        return false;
    }
    std::fs::rename(stage_dir, lock_dir).is_ok()
}

/// Sweep any leftover `.approvals.lock.stage.<pid>-*` directories left behind
/// by a process that terminated before publishing its staged lock.
fn cleanup_stale_stage_dirs(dir: &Path) {
    let stage_prefix = format!("{WHEEL_LOCK_DIR_NAME}.stage.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.take(32).flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&stage_prefix) else {
            continue;
        };
        let Some(pid_str) = rest.split('-').next() else {
            continue;
        };
        let Ok(stage_pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if stage_pid >= 1 && !is_lock_owner_alive(stage_pid) {
            remove_lock_dir_contents(&entry.path());
        }
    }
}

/// Reclaim a dead-owner `.approvals.lock` without renaming `lock_dir` or
/// removing an ownerless directory.
///
/// Reclamation only unlinks a verified-dead `owner.<token>` (or legacy `owner`)
/// file observed in `lock_dir` and then calls `remove_dir(lock_dir)` only when
/// a dead entry was unlinked, which the OS kernel guarantees fails with
/// `ENOTEMPTY` if another contender has already published a replacement lock
/// containing a different `owner.<new_token>` entry.
fn try_reclaim_stale_lock_with_hook<R>(
    dir: &Path,
    lock_dir: &Path,
    before_reclaim_unlink: &mut R,
) -> bool
where
    R: FnMut(&Path),
{
    cleanup_stale_stage_dirs(dir);
    let Ok(entries) = std::fs::read_dir(lock_dir) else {
        return false;
    };
    let mut unlinked_dead_entry = false;
    for entry in entries.take(16).flatten() {
        let entry_path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "owner" {
            let Some(owner_pid) = read_lock_owner_pid(&entry_path) else {
                continue;
            };
            if is_lock_owner_alive(owner_pid) {
                return false;
            }
            let tagged_path = lock_dir.join(format!("owner.dead.{owner_pid}"));
            if std::fs::rename(&entry_path, &tagged_path).is_ok() {
                if read_lock_owner_pid(&tagged_path) == Some(owner_pid) {
                    before_reclaim_unlink(&tagged_path);
                    let _ = std::fs::remove_file(&tagged_path);
                    unlinked_dead_entry = true;
                } else {
                    let _ = std::fs::rename(&tagged_path, &entry_path);
                    return false;
                }
            }
        } else if name.starts_with("owner.") {
            let Some(owner_pid) = read_lock_owner_pid(&entry_path) else {
                continue;
            };
            if is_lock_owner_alive(owner_pid) {
                return false;
            }
            before_reclaim_unlink(&entry_path);
            if std::fs::remove_file(&entry_path).is_ok() {
                unlinked_dead_entry = true;
            }
        }
    }
    if unlinked_dead_entry && std::fs::remove_dir(lock_dir).is_ok() {
        return true;
    }
    unlinked_dead_entry && !lock_dir.exists()
}

struct WheelStoreLockGuard {
    lock_dir: std::path::PathBuf,
    owner_file: std::path::PathBuf,
}

impl Drop for WheelStoreLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.owner_file);
        let _ = std::fs::remove_dir(&self.lock_dir);
    }
}

/// Serialize read-check-replace mutations across processes using `.approvals.lock`.
fn with_wheel_store_lock<T, F>(dir: &Path, f: F) -> Result<T, ConfigWriteFailure>
where
    F: FnOnce() -> Result<T, ConfigWriteFailure>,
{
    with_wheel_store_lock_with_hooks(dir, |_| {}, |_, _| {}, |_| {}, |_| {}, f)
}

fn with_wheel_store_lock_with_hooks<T, F, P, A, R, C>(
    dir: &Path,
    mut before_publish: P,
    mut after_publish: A,
    mut before_reclaim_unlink: R,
    mut after_reclaim: C,
    f: F,
) -> Result<T, ConfigWriteFailure>
where
    F: FnOnce() -> Result<T, ConfigWriteFailure>,
    P: FnMut(usize),
    A: FnMut(usize, bool),
    R: FnMut(&Path),
    C: FnMut(bool),
{
    ensure_private_dir(dir)?;
    let lock_dir = dir.join(WHEEL_LOCK_DIR_NAME);
    let pid = std::process::id();
    let seq = WHEEL_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let token = format!("{pid}-{nanos}-{seq}");
    let owner_filename = format!("owner.{token}");
    let stage_dir = dir.join(format!("{WHEEL_LOCK_DIR_NAME}.stage.{token}"));

    if prepare_staged_lock_dir(&stage_dir, &owner_filename, pid).is_err() {
        return Err(ConfigWriteFailure::Io);
    }

    let owner_file = lock_dir.join(&owner_filename);
    let mut acquired = false;
    for attempt in 1..=WHEEL_LOCK_MAX_ATTEMPTS {
        before_publish(attempt);
        let published = publish_staged_lock_dir(&stage_dir, &lock_dir);
        after_publish(attempt, published);
        if published {
            if read_lock_owner_pid(&owner_file) == Some(pid) {
                acquired = true;
                break;
            }
            let _ = std::fs::remove_file(&owner_file);
            let _ = std::fs::remove_dir(&lock_dir);
            return Err(ConfigWriteFailure::Io);
        }
        if attempt >= WHEEL_LOCK_RECLAIM_ATTEMPT {
            let reclaimed =
                try_reclaim_stale_lock_with_hook(dir, &lock_dir, &mut before_reclaim_unlink);
            after_reclaim(reclaimed);
            if reclaimed {
                continue;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(WHEEL_LOCK_SLEEP_MS));
    }
    if !acquired {
        remove_lock_dir_contents(&stage_dir);
        return Err(ConfigWriteFailure::Stale);
    }
    let _guard = WheelStoreLockGuard {
        lock_dir,
        owner_file,
    };
    f()
}

/// Atomically write `bytes` to `dir/filename` with `0700` dir and `0600` file permissions.
fn write_private_file_atomic(
    dir: &Path,
    filename: &str,
    bytes: &[u8],
) -> Result<(), ConfigWriteFailure> {
    if bytes.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    ensure_private_dir(dir)?;
    let target = dir.join(filename);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(ConfigWriteFailure::Unsafe);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                if meta.uid() != nix::unistd::geteuid().as_raw() {
                    return Err(ConfigWriteFailure::Unsafe);
                }
                if (meta.permissions().mode() & 0o077) != 0 {
                    return Err(ConfigWriteFailure::Unsafe);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ConfigWriteFailure::Io),
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = WHEEL_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        "{WHEEL_TEMP_PREFIX}{}-{nanos}-{seq}.tmp",
        std::process::id()
    ));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(WHEEL_FILE_MODE);
    }
    let mut file = options.open(&temp).map_err(|_| ConfigWriteFailure::Io)?;
    let write_res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(WHEEL_FILE_MODE))?;
        }
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(ConfigWriteFailure::Io);
    }
    drop(file);

    let rename_res = match std::fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&target).and_then(|()| std::fs::rename(&temp, &target))
        }
        Err(err) => Err(err),
    };
    if rename_res.is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(ConfigWriteFailure::Io);
    }
    #[cfg(unix)]
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Non-evaluating inspection of a Lua source file for simple `key = "value"` or `key = bool` assignments.
/// Ignores lines whose first non-whitespace token starts with `--`.
fn scan_lua_assignments(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return out;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > WHEEL_MAX_FILE_BYTES as u64
    {
        return out;
    }
    let Ok(raw) = read_bounded_utf8_file(path) else {
        return out;
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        let Some((lhs, rhs)) = trimmed.split_once('=') else {
            continue;
        };
        let key = lhs.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let mut val = rhs.trim();
        // Strip trailing inline comment outside quotes if any, and trailing comma.
        if let Some(quoted) = extract_quoted_literal(val) {
            out.insert(key.to_string(), quoted);
            continue;
        }
        if let Some(comment_idx) = val.find("--") {
            val = val[..comment_idx].trim();
        }
        val = val.trim_end_matches(',').trim();
        if matches!(val, "true" | "false") {
            out.insert(key.to_string(), val.to_string());
        }
    }
    out
}

fn extract_quoted_literal(rhs: &str) -> Option<String> {
    let bytes = rhs.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == quote {
            return Some(out);
        }
        if b == b'\\' {
            i += 1;
            let esc = *bytes.get(i)?;
            out.push(char::from(esc));
        } else {
            let width = utf8_char_width(b)?;
            let slice = std::str::from_utf8(bytes.get(i..i + width)?).ok()?;
            out.push_str(slice);
            i += width;
            continue;
        }
        i += 1;
    }
    None
}

fn default_settings_object() -> BTreeMap<String, JsonValue> {
    let mut map = BTreeMap::new();
    map.insert(
        "schema_version".to_string(),
        JsonValue::String("1.0".to_string()),
    );
    map
}

fn default_credentials_object() -> BTreeMap<String, JsonValue> {
    let mut map = BTreeMap::new();
    map.insert(
        "schema_version".to_string(),
        JsonValue::String("1.0".to_string()),
    );
    map.insert("providers".to_string(), JsonValue::Object(BTreeMap::new()));
    map
}

/// Build the full [`WheelSnapshot`] from `$XDG_CONFIG_HOME/wheel`, `$XDG_DATA_HOME/wheel`,
/// and the injected boolean environment probe.
pub(crate) fn read_wheel_snapshot(
    wheel_config_dir: Option<&Path>,
    wheel_data_dir: Option<&Path>,
    env_probe: &dyn Fn(&str) -> bool,
) -> Result<WheelSnapshot, ConfigReadFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigReadFailure::Unavailable)?;
    let settings_map = read_private_json_for_write(data_dir, "settings.json")
        .map_err(map_write_to_read_failure)?
        .map(|(_, m)| m)
        .unwrap_or_default();
    let creds_map = read_private_json_for_write(data_dir, "credentials.json")
        .map_err(map_write_to_read_failure)?
        .map(|(_, m)| m)
        .unwrap_or_default();
    let approvals_map = read_private_json_for_write(data_dir, "approvals.json")
        .map_err(map_write_to_read_failure)?
        .map(|(_, m)| m)
        .unwrap_or_default();

    let global_init = wheel_config_dir
        .map(|dir| scan_lua_assignments(&dir.join("init.lua")))
        .unwrap_or_default();

    let default_provider = global_init
        .get("default_provider")
        .filter(|s| !s.is_empty())
        .cloned()
        .or_else(|| {
            settings_map
                .get("default_provider")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "opencode-go".to_string());

    let default_model = global_init
        .get("default_model")
        .cloned()
        .or_else(|| {
            settings_map
                .get("default_model")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut thinking_gear = global_init
        .get("thinking_gear")
        .filter(|g| {
            matches!(
                g.as_str(),
                "off" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        })
        .cloned()
        .or_else(|| {
            settings_map
                .get("thinking_gear")
                .and_then(JsonValue::as_str)
                .filter(|g| matches!(*g, "off" | "low" | "medium" | "high" | "xhigh" | "max"))
                .map(str::to_string)
        })
        .or_else(|| {
            settings_map
                .get("thinking")
                .and_then(JsonValue::as_str)
                .filter(|g| matches!(*g, "low" | "medium" | "high" | "xhigh" | "max"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "medium".to_string());

    let thinking_enabled = if let Some(flag_str) = global_init.get("thinking_enabled") {
        flag_str == "true"
    } else if let Some(flag) = settings_map
        .get("thinking_enabled")
        .and_then(JsonValue::as_bool)
    {
        flag
    } else if let Some(legacy) = settings_map.get("thinking").and_then(JsonValue::as_str) {
        matches!(legacy, "low" | "medium" | "high" | "xhigh" | "max")
    } else {
        false
    };
    if thinking_gear == "off" && thinking_enabled {
        thinking_gear = "medium".to_string();
    }

    let approval_mode = global_init
        .get("approval_mode")
        .filter(|m| {
            matches!(
                m.as_str(),
                "prompt" | "terminal" | "dashboard" | "auto-allow" | "auto-deny"
            )
        })
        .cloned()
        .or_else(|| {
            settings_map
                .get("approval_mode")
                .and_then(JsonValue::as_str)
                .filter(|m| {
                    matches!(
                        *m,
                        "prompt" | "terminal" | "dashboard" | "auto-allow" | "auto-deny"
                    )
                })
                .map(str::to_string)
        })
        .unwrap_or_else(|| "prompt".to_string());

    let settings_providers = settings_map.get("providers").and_then(JsonValue::as_object);
    let creds_providers = creds_map.get("providers").and_then(JsonValue::as_object);

    let mut providers = Vec::with_capacity(BUILTIN_PROVIDERS.len());
    for spec in BUILTIN_PROVIDERS {
        let lua_provider = wheel_config_dir
            .map(|dir| {
                scan_lua_assignments(&dir.join("providers").join(format!("{}.lua", spec.name)))
            })
            .unwrap_or_default();

        let (model, model_source) =
            if let Some(m) = lua_provider.get("model").filter(|s| !s.is_empty()) {
                (m.clone(), "global_config".to_string())
            } else if let Some(m) = settings_providers
                .and_then(|p| p.get(spec.name))
                .and_then(JsonValue::as_object)
                .and_then(|o| o.get("model"))
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
            {
                (m.to_string(), "data_settings".to_string())
            } else {
                (spec.default_model.to_string(), "default".to_string())
            };

        let (has_auth, auth_source) = if spec.auth_env.is_empty() {
            (true, "none".to_string())
        } else if env_probe(spec.auth_env) {
            (true, "env".to_string())
        } else if lua_provider.get("api_key").is_some_and(|k| !k.is_empty()) {
            (true, "global_config".to_string())
        } else if creds_providers
            .and_then(|p| p.get(spec.name))
            .and_then(JsonValue::as_object)
            .and_then(|o| o.get("api_key"))
            .and_then(JsonValue::as_str)
            .is_some_and(|k| !k.is_empty())
        {
            (true, "data_credentials".to_string())
        } else {
            (false, "none".to_string())
        };

        providers.push(WheelProviderRow {
            name: spec.name.to_string(),
            label: spec.label.to_string(),
            protocol: spec.protocol.to_string(),
            default_model: spec.default_model.to_string(),
            model,
            model_source,
            auth_env: spec.auth_env.to_string(),
            has_auth,
            auth_source,
            base_url: spec.base_url.to_string(),
        });
    }

    let settings_perms = settings_map
        .get("permissions")
        .and_then(JsonValue::as_object);
    let mut permissions = Vec::with_capacity(RECOGNIZED_PERMISSION_TOOLS.len());
    for &(tool, default_mode) in BUILTIN_PERMISSIONS {
        let mode = settings_perms
            .and_then(|p| p.get(tool))
            .and_then(JsonValue::as_str)
            .filter(|m| matches!(*m, "auto" | "ask" | "deny"))
            .unwrap_or(default_mode);
        permissions.push(WheelPermissionRow {
            tool: tool.to_string(),
            mode: mode.to_string(),
        });
    }
    if let Some(perms_obj) = settings_perms {
        for &extra_tool in &["fs_read", "fs_write", "run_command", "git", "network"] {
            if let Some(mode) = perms_obj
                .get(extra_tool)
                .and_then(JsonValue::as_str)
                .filter(|m| matches!(*m, "auto" | "ask" | "deny"))
            {
                permissions.push(WheelPermissionRow {
                    tool: extra_tool.to_string(),
                    mode: mode.to_string(),
                });
            }
        }
    }

    let mut approvals = Vec::new();
    if let Some(JsonValue::Array(items)) = approvals_map.get("entries") {
        for item in items.iter().take(WHEEL_APPROVALS_MAX_ENTRIES) {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(id) = obj.get("id").and_then(JsonValue::as_str) else {
                continue;
            };
            let session_id = obj
                .get("session_id")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string();
            let tool = obj
                .get("tool")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown")
                .to_string();
            let risk = obj
                .get("risk")
                .and_then(JsonValue::as_str)
                .unwrap_or("high")
                .to_string();
            let summary = obj
                .get("summary")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>();
            let status = obj
                .get("status")
                .and_then(JsonValue::as_str)
                .unwrap_or("pending")
                .to_string();
            let decision = obj
                .get("decision")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let created_at = obj
                .get("created_at")
                .and_then(JsonValue::as_i64)
                .unwrap_or(0);
            approvals.push(WheelApprovalRow {
                id: id.to_string(),
                session_id,
                tool,
                risk,
                summary,
                status,
                decision,
                created_at,
            });
        }
    }

    Ok(WheelSnapshot {
        default_provider,
        default_model,
        thinking_enabled,
        thinking_gear,
        approval_mode,
        providers,
        permissions,
        approvals,
    })
}

struct PreparedWheelWrite {
    serialized: String,
    rendered: String,
    candidate_digest: String,
    changed: bool,
    changed_keys: Vec<String>,
}

fn prepare_wheel_setting_write(
    wheel_data_dir: Option<&Path>,
    key: &str,
    value: &WheelSettingValue,
) -> Result<PreparedWheelWrite, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    let existing = read_private_json_for_write(data_dir, "settings.json")?;
    let mut map = existing
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_else(default_settings_object);
    if !map.contains_key("schema_version") {
        map.insert(
            "schema_version".to_string(),
            JsonValue::String("1.0".to_string()),
        );
    }
    let before_canonical = serialize_json(&JsonValue::Object(map.clone()));

    match (key, value) {
        ("default_provider" | "default_model" | "approval_mode", WheelSettingValue::Text(s)) => {
            map.insert(key.to_string(), JsonValue::String(s.clone()));
        }
        ("thinking_enabled", WheelSettingValue::Bool(enabled)) => {
            map.insert("thinking_enabled".to_string(), JsonValue::Bool(*enabled));
            let current_gear = map
                .get("thinking_gear")
                .and_then(JsonValue::as_str)
                .filter(|g| matches!(*g, "low" | "medium" | "high" | "xhigh" | "max"))
                .or_else(|| {
                    map.get("thinking")
                        .and_then(JsonValue::as_str)
                        .filter(|g| matches!(*g, "low" | "medium" | "high" | "xhigh" | "max"))
                })
                .unwrap_or("medium")
                .to_string();
            if *enabled {
                map.insert(
                    "thinking_gear".to_string(),
                    JsonValue::String(current_gear.clone()),
                );
                map.insert("thinking".to_string(), JsonValue::String(current_gear));
            } else {
                map.insert("thinking".to_string(), JsonValue::String("off".to_string()));
            }
        }
        ("thinking_gear", WheelSettingValue::Text(gear)) => {
            map.insert("thinking_gear".to_string(), JsonValue::String(gear.clone()));
            if gear == "off" {
                map.insert("thinking_enabled".to_string(), JsonValue::Bool(false));
                map.insert("thinking".to_string(), JsonValue::String("off".to_string()));
            } else {
                let enabled = map
                    .get("thinking_enabled")
                    .and_then(JsonValue::as_bool)
                    .or_else(|| {
                        map.get("thinking")
                            .and_then(JsonValue::as_str)
                            .map(|legacy| {
                                matches!(legacy, "low" | "medium" | "high" | "xhigh" | "max")
                            })
                    })
                    .unwrap_or(false);
                if enabled {
                    map.insert("thinking".to_string(), JsonValue::String(gear.clone()));
                } else {
                    map.insert("thinking".to_string(), JsonValue::String("off".to_string()));
                }
            }
        }
        (_, WheelSettingValue::Text(mode)) if key.starts_with("permissions.") => {
            let Some(tool) = key.strip_prefix("permissions.") else {
                return Err(ConfigWriteFailure::Invalid);
            };
            if tool.is_empty() || !RECOGNIZED_PERMISSION_TOOLS.contains(&tool) {
                return Err(ConfigWriteFailure::Invalid);
            }
            let perms_entry = map
                .entry("permissions".to_string())
                .or_insert_with(|| JsonValue::Object(BTreeMap::new()));
            let Some(perms_obj) = perms_entry.as_object_mut() else {
                return Err(ConfigWriteFailure::Invalid);
            };
            perms_obj.insert(tool.to_string(), JsonValue::String(mode.clone()));
        }
        (_, WheelSettingValue::Text(model))
            if key.starts_with("providers.") && key.ends_with(".model") =>
        {
            let Some(provider) = key
                .strip_prefix("providers.")
                .and_then(|rest| rest.strip_suffix(".model"))
            else {
                return Err(ConfigWriteFailure::Invalid);
            };
            if provider.is_empty() || !BUILTIN_PROVIDERS.iter().any(|spec| spec.name == provider) {
                return Err(ConfigWriteFailure::Invalid);
            }
            let providers_entry = map
                .entry("providers".to_string())
                .or_insert_with(|| JsonValue::Object(BTreeMap::new()));
            let Some(providers_obj) = providers_entry.as_object_mut() else {
                return Err(ConfigWriteFailure::Invalid);
            };
            let p_entry = providers_obj
                .entry(provider.to_string())
                .or_insert_with(|| JsonValue::Object(BTreeMap::new()));
            let Some(p_obj) = p_entry.as_object_mut() else {
                return Err(ConfigWriteFailure::Invalid);
            };
            p_obj.insert("model".to_string(), JsonValue::String(model.clone()));
        }
        _ => return Err(ConfigWriteFailure::Invalid),
    }

    let serialized = serialize_json(&JsonValue::Object(map));
    if serialized.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    let changed = existing.is_none() || before_canonical != serialized;
    let rendered = format!("{key} = {}", value.render_literal());
    let digest = candidate_digest(serialized.as_bytes());
    let changed_keys = if changed {
        vec![key.to_string()]
    } else {
        Vec::new()
    };
    Ok(PreparedWheelWrite {
        serialized,
        rendered,
        candidate_digest: digest,
        changed,
        changed_keys,
    })
}

pub(crate) fn preview_wheel_set(
    wheel_data_dir: Option<&Path>,
    key: &str,
    value: &WheelSettingValue,
) -> Result<WheelMutationRendering, ConfigWriteFailure> {
    let prepared = prepare_wheel_setting_write(wheel_data_dir, key, value)?;
    Ok(WheelMutationRendering {
        changed_keys: prepared.changed_keys,
        rendered: prepared.rendered,
        candidate_digest: prepared.candidate_digest,
        changed: prepared.changed,
    })
}

pub(crate) fn apply_wheel_set(
    wheel_data_dir: Option<&Path>,
    key: &str,
    value: &WheelSettingValue,
    expected_candidate_digest: &str,
) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    with_wheel_store_lock(data_dir, || {
        let prepared = prepare_wheel_setting_write(wheel_data_dir, key, value)?;
        if prepared.candidate_digest != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        write_private_file_atomic(data_dir, "settings.json", prepared.serialized.as_bytes())?;
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: false,
        })
    })
}

fn prepare_wheel_auth_set_write(
    wheel_data_dir: Option<&Path>,
    provider: &str,
    api_key: &str,
) -> Result<PreparedWheelWrite, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    let existing = read_private_json_for_write(data_dir, "credentials.json")?;
    let mut map = existing
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_else(default_credentials_object);
    if !map.contains_key("schema_version") {
        map.insert(
            "schema_version".to_string(),
            JsonValue::String("1.0".to_string()),
        );
    }
    let before_canonical = serialize_json(&JsonValue::Object(map.clone()));

    let providers_entry = map
        .entry("providers".to_string())
        .or_insert_with(|| JsonValue::Object(BTreeMap::new()));
    let Some(providers_obj) = providers_entry.as_object_mut() else {
        return Err(ConfigWriteFailure::Invalid);
    };
    let p_entry = providers_obj
        .entry(provider.to_string())
        .or_insert_with(|| JsonValue::Object(BTreeMap::new()));
    let Some(p_obj) = p_entry.as_object_mut() else {
        return Err(ConfigWriteFailure::Invalid);
    };
    p_obj.insert(
        "api_key".to_string(),
        JsonValue::String(api_key.to_string()),
    );

    let serialized = serialize_json(&JsonValue::Object(map));
    if serialized.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    let changed = existing.is_none() || before_canonical != serialized;
    let logical_key = format!("providers.{provider}.api_key");
    let rendered = format!("{logical_key} = \"***REDACTED***\"");
    let digest = candidate_digest(serialized.as_bytes());
    let changed_keys = if changed {
        vec![logical_key]
    } else {
        Vec::new()
    };
    Ok(PreparedWheelWrite {
        serialized,
        rendered,
        candidate_digest: digest,
        changed,
        changed_keys,
    })
}

pub(crate) fn preview_wheel_auth_set(
    wheel_data_dir: Option<&Path>,
    provider: &str,
    api_key: &str,
) -> Result<WheelMutationRendering, ConfigWriteFailure> {
    let prepared = prepare_wheel_auth_set_write(wheel_data_dir, provider, api_key)?;
    Ok(WheelMutationRendering {
        changed_keys: prepared.changed_keys,
        rendered: prepared.rendered,
        candidate_digest: prepared.candidate_digest,
        changed: prepared.changed,
    })
}

pub(crate) fn apply_wheel_auth_set(
    wheel_data_dir: Option<&Path>,
    provider: &str,
    api_key: &str,
    expected_candidate_digest: &str,
) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    with_wheel_store_lock(data_dir, || {
        let prepared = prepare_wheel_auth_set_write(wheel_data_dir, provider, api_key)?;
        if prepared.candidate_digest != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        write_private_file_atomic(data_dir, "credentials.json", prepared.serialized.as_bytes())?;
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: false,
        })
    })
}

fn prepare_wheel_auth_remove_write(
    wheel_data_dir: Option<&Path>,
    provider: &str,
) -> Result<PreparedWheelWrite, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    let existing = read_private_json_for_write(data_dir, "credentials.json")?;
    let mut map = existing
        .as_ref()
        .map(|(_, m)| m.clone())
        .unwrap_or_else(default_credentials_object);
    if !map.contains_key("schema_version") {
        map.insert(
            "schema_version".to_string(),
            JsonValue::String("1.0".to_string()),
        );
    }
    let before_canonical = serialize_json(&JsonValue::Object(map.clone()));

    let mut had_key = false;
    if let Some(providers_obj) = map.get_mut("providers").and_then(JsonValue::as_object_mut) {
        if providers_obj.remove(provider).is_some() {
            had_key = true;
        }
    }

    let serialized = serialize_json(&JsonValue::Object(map));
    if serialized.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    let changed = had_key && before_canonical != serialized;
    let logical_key = format!("providers.{provider}.api_key");
    let rendered = format!("- {logical_key} = \"***REDACTED***\"");
    let digest = candidate_digest(serialized.as_bytes());
    let changed_keys = if changed {
        vec![logical_key]
    } else {
        Vec::new()
    };
    Ok(PreparedWheelWrite {
        serialized,
        rendered,
        candidate_digest: digest,
        changed,
        changed_keys,
    })
}

pub(crate) fn preview_wheel_auth_remove(
    wheel_data_dir: Option<&Path>,
    provider: &str,
) -> Result<WheelMutationRendering, ConfigWriteFailure> {
    let prepared = prepare_wheel_auth_remove_write(wheel_data_dir, provider)?;
    Ok(WheelMutationRendering {
        changed_keys: prepared.changed_keys,
        rendered: prepared.rendered,
        candidate_digest: prepared.candidate_digest,
        changed: prepared.changed,
    })
}

pub(crate) fn apply_wheel_auth_remove(
    wheel_data_dir: Option<&Path>,
    provider: &str,
    expected_candidate_digest: &str,
) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    with_wheel_store_lock(data_dir, || {
        let prepared = prepare_wheel_auth_remove_write(wheel_data_dir, provider)?;
        if prepared.candidate_digest != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        write_private_file_atomic(data_dir, "credentials.json", prepared.serialized.as_bytes())?;
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: false,
        })
    })
}

fn prepare_wheel_resolve_approval_write(
    wheel_data_dir: Option<&Path>,
    id: &str,
    decision: &str,
) -> Result<PreparedWheelWrite, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    let Some((raw_before, mut map)) = read_private_json_for_write(data_dir, "approvals.json")?
    else {
        return Err(ConfigWriteFailure::Invalid);
    };
    let entries_val = map.get_mut("entries").ok_or(ConfigWriteFailure::Invalid)?;
    let entries = match entries_val {
        JsonValue::Array(arr) => arr,
        JsonValue::Object(empty_obj) if empty_obj.is_empty() => {
            return Err(ConfigWriteFailure::Invalid);
        }
        _ => return Err(ConfigWriteFailure::Invalid),
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut found = false;
    let mut changed = false;
    for item in entries.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        if obj.get("id").and_then(JsonValue::as_str) == Some(id) {
            found = true;
            let prev_status = obj.get("status").and_then(JsonValue::as_str).unwrap_or("");
            let prev_decision = obj
                .get("decision")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            if prev_status != "resolved" || prev_decision != decision {
                changed = true;
                obj.insert(
                    "status".to_string(),
                    JsonValue::String("resolved".to_string()),
                );
                obj.insert(
                    "decision".to_string(),
                    JsonValue::String(decision.to_string()),
                );
                obj.insert("updated_at".to_string(), JsonValue::Int(now_secs));
            }
            break;
        }
    }
    if !found {
        return Err(ConfigWriteFailure::Invalid);
    }

    let serialized = serialize_json(&JsonValue::Object(map));
    if serialized.len() > WHEEL_MAX_FILE_BYTES {
        return Err(ConfigWriteFailure::Invalid);
    }
    // Bind candidate_digest to the exact on-disk preimage + (id, decision) so
    // wall-clock `updated_at` advancement between preview and apply never causes
    // a false Stale failure, while any concurrent modification to approvals.json
    // is caught immediately.
    let preimage_binding = format!("{raw_before}\0{id}\0{decision}");
    let digest = candidate_digest(preimage_binding.as_bytes());
    let logical_key = format!("approvals.{id}.decision");
    let rendered = format!("{logical_key} = {decision:?}");
    let changed_keys = if changed {
        vec![logical_key]
    } else {
        Vec::new()
    };
    Ok(PreparedWheelWrite {
        serialized,
        rendered,
        candidate_digest: digest,
        changed,
        changed_keys,
    })
}

pub(crate) fn preview_wheel_resolve_approval(
    wheel_data_dir: Option<&Path>,
    id: &str,
    decision: &str,
) -> Result<WheelMutationRendering, ConfigWriteFailure> {
    let prepared = prepare_wheel_resolve_approval_write(wheel_data_dir, id, decision)?;
    Ok(WheelMutationRendering {
        changed_keys: prepared.changed_keys,
        rendered: prepared.rendered,
        candidate_digest: prepared.candidate_digest,
        changed: prepared.changed,
    })
}

pub(crate) fn apply_wheel_resolve_approval(
    wheel_data_dir: Option<&Path>,
    id: &str,
    decision: &str,
    expected_candidate_digest: &str,
) -> Result<ConfigApplyOutcome, ConfigWriteFailure> {
    let data_dir = wheel_data_dir.ok_or(ConfigWriteFailure::Unavailable)?;
    with_wheel_store_lock(data_dir, || {
        let prepared = prepare_wheel_resolve_approval_write(wheel_data_dir, id, decision)?;
        if prepared.candidate_digest != expected_candidate_digest {
            return Err(ConfigWriteFailure::Stale);
        }
        if !prepared.changed {
            return Ok(ConfigApplyOutcome::Unchanged);
        }
        write_private_file_atomic(data_dir, "approvals.json", prepared.serialized.as_bytes())?;
        Ok(ConfigApplyOutcome::Applied {
            pending_restart: false,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = WHEEL_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-wheel-manage-test-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        ensure_private_dir(&dir).expect("create private temp dir");
        dir
    }

    #[test]
    fn json_roundtrip_preserves_unknown_fields_and_escapes() {
        let raw = r#"{"extra":{"nested":[1,true,null,"line\nbreak"]},"schema_version":"1.0"}"#;
        let parsed = parse_json(raw).expect("valid json");
        let serialized = serialize_json(&parsed);
        let reparsed = parse_json(&serialized).expect("reparse serialized");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn wheel_precedence_and_redacted_auth_lifecycle() {
        let root = temp_dir("precedence");
        let cfg_dir = root.join("config-wheel");
        let data_dir = root.join("data-wheel");
        ensure_private_dir(&cfg_dir).unwrap();
        ensure_private_dir(&cfg_dir.join("providers")).unwrap();
        ensure_private_dir(&data_dir).unwrap();

        // Starter opencode-go.lua with commented model (as generated by `wheel init`)
        std::fs::write(
            cfg_dir.join("providers").join("opencode-go.lua"),
            "return {\n  -- model = \"deepseek-v4.1-flash\",\n}\n",
        )
        .unwrap();
        // Global openai.lua with uncommented model and api_key
        std::fs::write(
            cfg_dir.join("providers").join("openai.lua"),
            "return {\n  model = \"gpt-5.4-mini\",\n  api_key = \"sk-global-never-leaked\",\n}\n",
        )
        .unwrap();

        let probe: Rc<dyn Fn(&str) -> bool> = Rc::new(|var| var == "ANTHROPIC_API_KEY");

        // Set opencode-go model via data_settings
        let prev_model = preview_wheel_set(
            Some(&data_dir),
            "providers.opencode-go.model",
            &WheelSettingValue::Text("kimi-k2.5".to_string()),
        )
        .unwrap();
        assert!(prev_model.changed);
        apply_wheel_set(
            Some(&data_dir),
            "providers.opencode-go.model",
            &WheelSettingValue::Text("kimi-k2.5".to_string()),
            &prev_model.candidate_digest,
        )
        .unwrap();

        // Set opencode-go api_key via data_credentials
        let raw_secret = "sk-opencode-super-secret-value";
        let prev_auth = preview_wheel_auth_set(Some(&data_dir), "opencode-go", raw_secret).unwrap();
        assert!(prev_auth.changed);
        assert!(!prev_auth.rendered.contains(raw_secret));
        assert!(prev_auth.rendered.contains("***REDACTED***"));
        apply_wheel_auth_set(
            Some(&data_dir),
            "opencode-go",
            raw_secret,
            &prev_auth.candidate_digest,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let creds_mode = std::fs::symlink_metadata(data_dir.join("credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(creds_mode, 0o600);
        }

        let snap = read_wheel_snapshot(Some(&cfg_dir), Some(&data_dir), &*probe).unwrap();
        let oc = snap
            .providers
            .iter()
            .find(|p| p.name == "opencode-go")
            .unwrap();
        assert_eq!(oc.model, "kimi-k2.5");
        assert_eq!(oc.model_source, "data_settings");
        assert!(oc.has_auth);
        assert_eq!(oc.auth_source, "data_credentials");

        let oa = snap.providers.iter().find(|p| p.name == "openai").unwrap();
        assert_eq!(oa.model, "gpt-5.4-mini");
        assert_eq!(oa.model_source, "global_config");
        assert!(oa.has_auth);
        assert_eq!(oa.auth_source, "global_config");

        let an = snap
            .providers
            .iter()
            .find(|p| p.name == "anthropic")
            .unwrap();
        assert!(an.has_auth);
        assert_eq!(an.auth_source, "env");

        // Remove opencode-go auth
        let prev_rm = preview_wheel_auth_remove(Some(&data_dir), "opencode-go").unwrap();
        assert!(prev_rm.changed);
        apply_wheel_auth_remove(Some(&data_dir), "opencode-go", &prev_rm.candidate_digest).unwrap();
        let snap_after = read_wheel_snapshot(Some(&cfg_dir), Some(&data_dir), &*probe).unwrap();
        let oc_after = snap_after
            .providers
            .iter()
            .find(|p| p.name == "opencode-go")
            .unwrap();
        assert!(!oc_after.has_auth);
        assert_eq!(oc_after.auth_source, "none");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wheel_thinking_sync_and_approval_resolution() {
        let root = temp_dir("approvals");
        let data_dir = root.join("data-wheel");
        ensure_private_dir(&data_dir).unwrap();

        // Toggle thinking_enabled = true and thinking_gear = "high"
        let p1 = preview_wheel_set(
            Some(&data_dir),
            "thinking_enabled",
            &WheelSettingValue::Bool(true),
        )
        .unwrap();
        apply_wheel_set(
            Some(&data_dir),
            "thinking_enabled",
            &WheelSettingValue::Bool(true),
            &p1.candidate_digest,
        )
        .unwrap();

        let p2 = preview_wheel_set(
            Some(&data_dir),
            "thinking_gear",
            &WheelSettingValue::Text("high".to_string()),
        )
        .unwrap();
        apply_wheel_set(
            Some(&data_dir),
            "thinking_gear",
            &WheelSettingValue::Text("high".to_string()),
            &p2.candidate_digest,
        )
        .unwrap();

        // Seed approvals.json
        let approvals_json = r#"{"schema_version":"1.0","entries":[{"id":"apr-101","session_id":"sess-1","tool":"bash","risk":"high","summary":"cargo test","status":"pending","created_at":1700000000}]}"#;
        write_private_file_atomic(&data_dir, "approvals.json", approvals_json.as_bytes()).unwrap();

        let prev_res =
            preview_wheel_resolve_approval(Some(&data_dir), "apr-101", "allow_once").unwrap();
        assert!(prev_res.changed);
        apply_wheel_resolve_approval(
            Some(&data_dir),
            "apr-101",
            "allow_once",
            &prev_res.candidate_digest,
        )
        .unwrap();

        let snap = read_wheel_snapshot(None, Some(&data_dir), &|_| false).unwrap();
        assert!(snap.thinking_enabled);
        assert_eq!(snap.thinking_gear, "high");
        assert_eq!(snap.approvals.len(), 1);
        assert_eq!(snap.approvals[0].status, "resolved");
        assert_eq!(snap.approvals[0].decision.as_deref(), Some("allow_once"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_surrogate_pairs_and_large_floats_roundtrip() {
        // Surrogate pair \uD83D\uDE00 -> U+1F600 (😀), plus finite float 1e20
        let raw = r#"{"emoji":"\uD83D\uDE00","extra":1e20,"schema_version":"1.0"}"#;
        let parsed = parse_json(raw).expect("valid surrogate pair and 1e20 float");
        let obj = parsed.as_object().expect("json object");
        assert_eq!(obj.get("emoji").and_then(JsonValue::as_str), Some("😀"));
        let serialized = serialize_json(&parsed);
        let reparsed = parse_json(&serialized).expect("reparse serialized 1e20 float");
        assert_eq!(parsed, reparsed);

        // Unpaired high or low surrogates must fail closed
        assert!(parse_json(r#"{"bad":"\uD83D"}"#).is_err());
        assert!(parse_json(r#"{"bad":"\uDE00"}"#).is_err());
        assert!(parse_json(r#"{"bad":"\uD83D\u0041"}"#).is_err());

        // Ensure an unrelated settings update preserves a document containing 1e20
        let root = temp_dir("float-preserve");
        let data_dir = root.join("data-wheel");
        ensure_private_dir(&data_dir).unwrap();
        write_private_file_atomic(&data_dir, "settings.json", raw.as_bytes()).unwrap();
        let prev = preview_wheel_set(
            Some(&data_dir),
            "approval_mode",
            &WheelSettingValue::Text("dashboard".to_string()),
        )
        .unwrap();
        apply_wheel_set(
            Some(&data_dir),
            "approval_mode",
            &WheelSettingValue::Text("dashboard".to_string()),
            &prev.candidate_digest,
        )
        .unwrap();
        let snap = read_wheel_snapshot(None, Some(&data_dir), &|_| false).unwrap();
        assert_eq!(snap.approval_mode, "dashboard");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_thinking_preserved_on_gear_change_and_invalid_keys_rejected() {
        let root = temp_dir("legacy-thinking");
        let data_dir = root.join("data-wheel");
        ensure_private_dir(&data_dir).unwrap();

        // Seed legacy {"thinking":"high"} without explicit thinking_enabled
        write_private_file_atomic(
            &data_dir,
            "settings.json",
            br#"{"schema_version":"1.0","thinking":"high"}"#,
        )
        .unwrap();

        let prev = preview_wheel_set(
            Some(&data_dir),
            "thinking_gear",
            &WheelSettingValue::Text("low".to_string()),
        )
        .unwrap();
        apply_wheel_set(
            Some(&data_dir),
            "thinking_gear",
            &WheelSettingValue::Text("low".to_string()),
            &prev.candidate_digest,
        )
        .unwrap();

        let snap = read_wheel_snapshot(None, Some(&data_dir), &|_| false).unwrap();
        assert!(snap.thinking_enabled);
        assert_eq!(snap.thinking_gear, "low");

        // Overlapping prefix/suffix key "providers.model" must fail closed without panicking
        assert_eq!(
            preview_wheel_set(
                Some(&data_dir),
                "providers.model",
                &WheelSettingValue::Text("gpt-5.4".to_string()),
            )
            .unwrap_err(),
            ConfigWriteFailure::Invalid
        );
        assert_eq!(
            preview_wheel_set(
                Some(&data_dir),
                "providers..model",
                &WheelSettingValue::Text("gpt-5.4".to_string()),
            )
            .unwrap_err(),
            ConfigWriteFailure::Invalid
        );

        // Candidate exceeding 64 KiB must be rejected before preview or write
        let near_limit = format!(
            r#"{{"schema_version":"1.0","pad":"{}"}}"#,
            "a".repeat(WHEEL_MAX_FILE_BYTES - 45)
        );
        write_private_file_atomic(&data_dir, "settings.json", near_limit.as_bytes()).unwrap();
        assert_eq!(
            preview_wheel_set(
                Some(&data_dir),
                "default_model",
                &WheelSettingValue::Text("x".repeat(64)),
            )
            .unwrap_err(),
            ConfigWriteFailure::Invalid
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reclaims_ownerless_and_concurrent_stale_store_locks() {
        let root = temp_dir("lock-reclaim");
        let data_dir = root.join("data-wheel");
        ensure_private_dir(&data_dir).unwrap();

        // 1. A live owner recorded in the owner file is accepted, and leftover dead
        //    staging directories plus dead-owner locks are reclaimed cleanly. The
        //    owner is this process so the liveness probe is portable; PID 1 is
        //    probed additionally on Unix, where the init process always exists.
        let self_pid = std::process::id();
        let live_owner_file = data_dir.join("live-owner");
        std::fs::write(&live_owner_file, format!("{self_pid}\n")).unwrap();
        assert_eq!(read_lock_owner_pid(&live_owner_file), Some(self_pid));
        assert!(is_lock_owner_alive(self_pid));
        #[cfg(unix)]
        {
            assert!(is_lock_owner_alive(1));
        }
        let _ = std::fs::remove_file(&live_owner_file);

        let dead_stage_dir = data_dir.join(format!("{WHEEL_LOCK_DIR_NAME}.stage.99999999-1-1"));
        prepare_staged_lock_dir(&dead_stage_dir, "owner.99999999-1-1", 99_999_999).unwrap();
        let lock_dir = data_dir.join(WHEEL_LOCK_DIR_NAME);
        create_private_lock_dir(&lock_dir).unwrap();
        std::fs::write(lock_dir.join("owner.99999999-stale-0"), "99999999\n").unwrap();
        let prev = preview_wheel_set(
            Some(&data_dir),
            "approval_mode",
            &WheelSettingValue::Text("dashboard".to_string()),
        )
        .unwrap();
        apply_wheel_set(
            Some(&data_dir),
            "approval_mode",
            &WheelSettingValue::Text("dashboard".to_string()),
            &prev.candidate_digest,
        )
        .unwrap();
        assert!(!lock_dir.exists());
        assert!(!dead_stage_dir.exists());

        // 2. Concurrent contenders racing to reclaim a dead-owner lock never delete
        //    another contender's replacement lock.
        create_private_lock_dir(&lock_dir).unwrap();
        std::fs::write(lock_dir.join("owner"), "99999999\n").unwrap();

        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlap = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let dir_clone = data_dir.clone();
            let active_clone = std::sync::Arc::clone(&active);
            let overlap_clone = std::sync::Arc::clone(&overlap);
            handles.push(std::thread::spawn(move || {
                with_wheel_store_lock(&dir_clone, || {
                    let prev_active =
                        active_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if prev_active > 0 {
                        overlap_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    active_clone.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(
            !overlap.load(std::sync::atomic::Ordering::SeqCst),
            "concurrent stale-lock reclamation allowed overlapping lock holders"
        );
        assert!(!lock_dir.exists());

        // 3. Deterministic pause before lock publication: Contender A stages its
        //    lock and pauses at a barrier before publishing to `lock_dir`.
        //    Contender B acquires the lock and enters the critical section before
        //    releasing Contender A. Contender B waits on `after_first_publish`
        //    while still holding the lock until Contender A has attempted its
        //    first publication and verified that the attempt failed (`!published`).
        let paused_before_publish = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume_publish = std::sync::Arc::new(std::sync::Barrier::new(2));
        let after_first_publish = std::sync::Arc::new(std::sync::Barrier::new(2));
        let b_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a_first_publish_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a_entered_while_b_active =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let dir_a = data_dir.clone();
        let paused_a = std::sync::Arc::clone(&paused_before_publish);
        let resume_a = std::sync::Arc::clone(&resume_publish);
        let after_pub_a = std::sync::Arc::clone(&after_first_publish);
        let b_done_a = std::sync::Arc::clone(&b_done);
        let first_failed_a = std::sync::Arc::clone(&a_first_publish_failed);
        let overlap_a = std::sync::Arc::clone(&a_entered_while_b_active);
        let handle_a = std::thread::spawn(move || {
            with_wheel_store_lock_with_hooks(
                &dir_a,
                |attempt| {
                    if attempt == 1 {
                        paused_a.wait();
                        resume_a.wait();
                    }
                },
                |attempt, published| {
                    if attempt == 1 {
                        first_failed_a.store(!published, std::sync::atomic::Ordering::SeqCst);
                        after_pub_a.wait();
                    }
                },
                |_| {},
                |_| {},
                || {
                    if !b_done_a.load(std::sync::atomic::Ordering::SeqCst) {
                        overlap_a.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(())
                },
            )
            .unwrap();
        });

        // Wait until Contender A has prepared its staged lock and paused before publish.
        paused_before_publish.wait();
        with_wheel_store_lock(&data_dir, || {
            // Wake Contender A while Contender B holds the lock in the critical section,
            // and wait until Contender A's first publication attempt has completed.
            resume_publish.wait();
            after_first_publish.wait();
            assert!(
                a_first_publish_failed.load(std::sync::atomic::Ordering::SeqCst),
                "Contender A's first publish attempt must fail while Contender B holds the lock"
            );
            assert!(
                lock_dir.exists(),
                "lock_dir must remain present while Contender B holds the lock"
            );
            b_done.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        handle_a.join().unwrap();
        assert!(
            !a_entered_while_b_active.load(std::sync::atomic::Ordering::SeqCst),
            "paused acquirer entered critical section while Contender B still held the lock"
        );
        assert!(!lock_dir.exists());

        // 4. Deterministic pause during stale-lock reclamation: seed a dead-owner
        //    entry `owner.99999999-stale`, pause Contender 1 right before it unlinks
        //    that stale entry, let Contender 2 reclaim the stale lock and acquire a
        //    live replacement lock, then release Contender 1 while Contender 2 is
        //    inside the critical section and wait on `after_first_reclaim` until
        //    Contender 1's delayed reclamation finishes before Contender 2 releases.
        create_private_lock_dir(&lock_dir).unwrap();
        std::fs::write(lock_dir.join("owner.99999999-stale"), "99999999\n").unwrap();
        let paused_in_reclaim = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume_reclaim = std::sync::Arc::new(std::sync::Barrier::new(2));
        let after_first_reclaim = std::sync::Arc::new(std::sync::Barrier::new(2));
        let c1_reclaim_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c2_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c1_overlapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let dir_c1 = data_dir.clone();
        let paused_c1 = std::sync::Arc::clone(&paused_in_reclaim);
        let resume_c1 = std::sync::Arc::clone(&resume_reclaim);
        let after_rec_c1 = std::sync::Arc::clone(&after_first_reclaim);
        let rec_failed_c1 = std::sync::Arc::clone(&c1_reclaim_failed);
        let c2_active_c1 = std::sync::Arc::clone(&c2_active);
        let overlap_c1 = std::sync::Arc::clone(&c1_overlapped);
        let mut paused_once = false;
        let mut notified_after_reclaim = false;
        let handle_c1 = std::thread::spawn(move || {
            with_wheel_store_lock_with_hooks(
                &dir_c1,
                |_| {},
                |_, _| {},
                |_| {
                    if !paused_once {
                        paused_once = true;
                        paused_c1.wait();
                        resume_c1.wait();
                    }
                },
                |reclaimed| {
                    if !notified_after_reclaim {
                        notified_after_reclaim = true;
                        rec_failed_c1.store(!reclaimed, std::sync::atomic::Ordering::SeqCst);
                        after_rec_c1.wait();
                    }
                },
                || {
                    if c2_active_c1.load(std::sync::atomic::Ordering::SeqCst) {
                        overlap_c1.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(())
                },
            )
            .unwrap();
        });

        // Wait until Contender 1 has identified `owner.99999999-stale` as dead and paused.
        paused_in_reclaim.wait();
        with_wheel_store_lock(&data_dir, || {
            c2_active.store(true, std::sync::atomic::Ordering::SeqCst);
            // Release Contender 1's delayed reclamation while Contender 2 holds the replacement lock,
            // and wait until Contender 1's delayed reclamation attempt finishes.
            resume_reclaim.wait();
            after_first_reclaim.wait();
            assert!(
                c1_reclaim_failed.load(std::sync::atomic::Ordering::SeqCst),
                "Contender 1's delayed reclamation must fail while Contender 2 holds the replacement lock"
            );
            assert!(
                lock_dir.exists(),
                "Contender 2's replacement lock must survive Contender 1's delayed reclamation"
            );
            c2_active.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        handle_c1.join().unwrap();
        assert!(
            !c1_overlapped.load(std::sync::atomic::Ordering::SeqCst),
            "paused reclaimer entered critical section concurrently with Contender 2"
        );
        assert!(!lock_dir.exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
